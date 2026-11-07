/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Stillwater: stop scripts that run too long without a break.
//!
//! A page that runs JavaScript in an endless loop (by bug or on purpose) used to hang its tab
//! for good: nothing else on that page could run, not even another script asking it to stop.
//! Firefox asks after 10 seconds whether to stop such a script. Stillwater stops it after
//! [`DEFAULT_LIMIT`] (or `STILLWATER_SLOW_SCRIPT_SECONDS`; 0 turns this off), after which the
//! page carries on with its next task.
//!
//! The limit is generous because pages are slower in Servo than in other browsers: some real
//! sites (Google Store, Wikidata) run one script for 40 seconds and then work fine, and stopping
//! them early breaks them.
//!
//! The script thread reports when it starts a task and when it goes back to waiting (the same
//! calls it makes to the background hang monitor, see [`WatchedHangMonitor`]). One watchdog
//! thread for all script threads checks once a second for a task that's been running longer
//! than the limit, and asks SpiderMonkey to interrupt it. The script thread's interrupt
//! callback then calls [`SlowScriptWatchdog::take_interrupt`] and stops the script.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, Weak};
use std::time::{Duration, Instant};

use background_hang_monitor_api::{BackgroundHangMonitor, HangAnnotation};
use log::warn;

use js::rust::ThreadSafeJSContext;

const DEFAULT_LIMIT: Duration = Duration::from_secs(60);
const CHECK_INTERVAL: Duration = Duration::from_millis(500);

static LIMIT: LazyLock<Option<Duration>> = LazyLock::new(|| {
    let seconds = std::env::var("STILLWATER_SLOW_SCRIPT_SECONDS")
        .ok()
        .and_then(|value| value.trim().parse::<f64>().ok());
    match seconds {
        Some(seconds) if seconds <= 0.0 => None,
        Some(seconds) => Some(Duration::from_secs_f64(seconds)),
        None => Some(DEFAULT_LIMIT),
    }
});
static EPOCH: LazyLock<Instant> = LazyLock::new(Instant::now);
static WATCHED: LazyLock<Mutex<Vec<Weak<State>>>> = LazyLock::new(|| {
    std::thread::Builder::new()
        .name("SlowScriptWatchdog".into())
        .spawn(watch)
        .expect("Couldn't start the slow script watchdog");
    Default::default()
});

fn now_ms() -> u64 {
    EPOCH.elapsed().as_millis() as u64 + 1
}

struct State {
    /// When the current task started, in ms since [`EPOCH`]; 0 while waiting for a task.
    busy_since: AtomicU64,
    /// Set by the watchdog when it asked to interrupt the current task.
    interrupt: AtomicBool,
    /// The script thread's JS context; `None` once the thread is shutting down.
    js_context: Mutex<Option<ThreadSafeJSContext>>,
}

/// One script thread's view of the watchdog.
pub(crate) struct SlowScriptWatchdog {
    state: Option<Arc<State>>,
}

impl SlowScriptWatchdog {
    pub(crate) fn new(js_context: ThreadSafeJSContext) -> Self {
        if LIMIT.is_none() {
            return SlowScriptWatchdog { state: None };
        }
        let state = Arc::new(State {
            busy_since: AtomicU64::new(0),
            interrupt: AtomicBool::new(false),
            js_context: Mutex::new(Some(js_context)),
        });
        WATCHED.lock().unwrap().push(Arc::downgrade(&state));
        SlowScriptWatchdog { state: Some(state) }
    }

    fn busy(&self) {
        if let Some(state) = &self.state {
            state.busy_since.store(now_ms(), Ordering::Relaxed);
            state.interrupt.store(false, Ordering::Relaxed);
        }
    }

    fn idle(&self) {
        if let Some(state) = &self.state {
            state.busy_since.store(0, Ordering::Relaxed);
            state.interrupt.store(false, Ordering::Relaxed);
        }
    }

    fn close(&self) {
        if let Some(state) = &self.state {
            state.js_context.lock().unwrap().take();
        }
    }

    /// Called from the interrupt callback: whether the running script should be stopped
    /// because it ran too long.
    pub(crate) fn take_interrupt(&self) -> bool {
        let Some(state) = &self.state else {
            return false;
        };
        if !state.interrupt.swap(false, Ordering::Relaxed) {
            return false;
        }
        // Give the rest of this task a fresh allowance, so code that runs after the stopped
        // script (other event listeners, say) isn't stopped straight away too.
        state.busy_since.store(now_ms(), Ordering::Relaxed);
        true
    }
}

impl Drop for SlowScriptWatchdog {
    fn drop(&mut self) {
        self.close();
    }
}

fn watch() {
    let Some(limit) = *LIMIT else {
        return;
    };
    let limit_ms = limit.as_millis() as u64;
    loop {
        std::thread::sleep(CHECK_INTERVAL);
        let now = now_ms();
        let states: Vec<Arc<State>> = {
            let mut watched = WATCHED.lock().unwrap();
            watched.retain(|state| state.strong_count() > 0);
            watched.iter().filter_map(Weak::upgrade).collect()
        };
        for state in states {
            let busy_since = state.busy_since.load(Ordering::Relaxed);
            if busy_since == 0 ||
                now.saturating_sub(busy_since) < limit_ms ||
                state.interrupt.load(Ordering::Relaxed)
            {
                continue;
            }
            let js_context = state.js_context.lock().unwrap();
            if let Some(js_context) = js_context.as_ref() {
                warn!(
                    "A script has been running for {:.1}s without a break; stopping it",
                    (now - busy_since) as f64 / 1000.0
                );
                state.interrupt.store(true, Ordering::Relaxed);
                js_context.request_interrupt_callback();
            }
        }
    }
}

/// The script thread's background hang monitor, also reporting to the [`SlowScriptWatchdog`].
pub(crate) struct WatchedHangMonitor {
    pub(crate) inner: Box<dyn BackgroundHangMonitor>,
    pub(crate) watchdog: SlowScriptWatchdog,
}

impl BackgroundHangMonitor for WatchedHangMonitor {
    fn notify_activity(&self, annotation: HangAnnotation) {
        self.watchdog.busy();
        self.inner.notify_activity(annotation);
    }

    fn notify_wait(&self) {
        self.watchdog.idle();
        self.inner.notify_wait();
    }

    fn unregister(&self) {
        self.watchdog.close();
        self.inner.unregister();
    }
}
