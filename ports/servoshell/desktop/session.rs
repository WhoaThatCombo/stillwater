/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Session restore: the ordinary tabs of every space are saved to `<config dir>/session.json`
//! while Stillwater runs, and come back as unloaded tabs on the next launch. The active tab of
//! the current space loads first; the others load in the background after it (Settings → On
//! startup, see `Workspaces::preload_tick`) or when clicked.
//!
//! The file is written by a background thread, at most about once a second, through a
//! temporary file that is renamed over the old one, so a crash loses at most a second of
//! changes and never leaves a half-written file.
//!
//! Format (version 1):
//! ```json
//! { "version": 1, "current": <space id>, "activeUrl": "<url of the active tab>",
//!   "activePin": <pin id or null>,
//!   "spaces": [ { "id": <space id>, "active": <index into tabs or null>,
//!                 "tabs": [ { "url", "title", "customTitle", "favicon",
//!                             "used": <ms since 1970 it was last active, optional> } ] } ] }
//! ```

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use log::warn;
use serde_json::Value;

const FILE_NAME: &str = "session.json";
/// Wait this long after a change for more changes before writing.
const WRITE_DELAY: Duration = Duration::from_millis(1000);

/// The previous session, read at startup, waiting for the sidebar to restore it.
static PENDING: Mutex<Option<PendingSession>> = Mutex::new(None);
/// Saving is allowed (a normal windowed launch, not WebDriver or headless).
static ENABLED: AtomicBool = AtomicBool::new(false);
/// The first window's sidebar has claimed the session file.
static CLAIMED: AtomicBool = AtomicBool::new(false);
/// The writer thread's channel, for `flush_from_any_thread` (cleared before the writer stops).
static WRITER: Mutex<Option<Sender<Message>>> = Mutex::new(None);

pub(crate) struct PendingSession {
    pub(crate) data: Value,
    /// The first tab Stillwater opens is the session's active tab (no URL was asked for).
    pub(crate) first_tab_is_active: bool,
}

fn path() -> Option<PathBuf> {
    crate::desktop::settings::config_dir().map(|dir| dir.join(FILE_NAME))
}

/// Called once at startup, before the window opens. `requested` is the URL given on the
/// command line (or by the app that launched Stillwater), if any. Returns the page the first
/// tab should open instead of the default one: the previous session's active tab.
pub(crate) fn init(requested: Option<&str>, headless: bool, webdriver: bool) -> Option<url::Url> {
    // STILLWATER_TEST_SESSION_RESTORE: save and restore the session under WebDriver too (for
    // testing session restore and background loading with a scratch profile).
    let test_restore = std::env::var_os("STILLWATER_TEST_SESSION_RESTORE").is_some();
    if headless || (webdriver && !test_restore) {
        return None;
    }
    ENABLED.store(true, Ordering::Relaxed);
    if !crate::desktop::settings::Settings::load().restore_session {
        return None;
    }
    let data = path()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .filter(|data| data["version"].as_u64() == Some(1))?;
    let requested = requested.filter(|url| !url.is_empty() && *url != "servo:newtab");
    let active = data["activeUrl"]
        .as_str()
        .and_then(|url| url::Url::parse(url).ok())
        .filter(|_| requested.is_none());
    crate::desktop::diagnostics::log(format!(
        "session: restoring {} tabs in {} spaces",
        count_tabs(&data),
        data["spaces"].as_array().map_or(0, Vec::len)
    ));
    if let Ok(mut pending) = PENDING.lock() {
        *pending = Some(PendingSession {
            data,
            first_tab_is_active: active.is_some(),
        });
    }
    active
}

fn count_tabs(data: &Value) -> usize {
    data["spaces"].as_array().map_or(0, |spaces| {
        spaces
            .iter()
            .map(|space| space["tabs"].as_array().map_or(0, Vec::len))
            .sum()
    })
}

/// The session to restore, for the first sidebar that asks.
pub(crate) fn take_pending() -> Option<PendingSession> {
    PENDING.lock().ok()?.take()
}

enum Message {
    Save(String),
    /// Write anything pending now, then reply.
    Flush(Sender<()>),
}

/// Saves the session in the background. Owned by the first window's sidebar; dropping it
/// writes whatever is pending.
pub(crate) struct SessionWriter {
    sender: Option<Sender<Message>>,
    thread: Option<JoinHandle<()>>,
    /// The last session handed to the thread, without favicons (to spot changes cheaply).
    last_signature: String,
}

impl SessionWriter {
    /// A writer for the first window that asks (other windows don't save their tabs).
    pub(crate) fn claim() -> Option<Self> {
        if !ENABLED.load(Ordering::Relaxed) || CLAIMED.swap(true, Ordering::Relaxed) {
            return None;
        }
        let path = path()?;
        let (sender, receiver) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("SessionWriter".into())
            .spawn(move || write_loop(&path, receiver))
            .ok()?;
        if let Ok(mut writer) = WRITER.lock() {
            *writer = Some(sender.clone());
        }
        Some(Self {
            sender: Some(sender),
            thread: Some(thread),
            last_signature: String::new(),
        })
    }

    /// Whether `signature` (the session without favicons) differs from the last one saved.
    /// If so, it is remembered and the caller should `save` the full session.
    pub(crate) fn changed(&mut self, signature: String) -> bool {
        if signature == self.last_signature {
            return false;
        }
        self.last_signature = signature;
        true
    }

    pub(crate) fn save(&self, session: String) {
        if let Some(sender) = &self.sender {
            let _ = sender.send(Message::Save(session));
        }
    }

    /// Write the pending session now (used by tests and before quitting).
    pub(crate) fn flush(&self) {
        let (reply, done) = mpsc::channel();
        if let Some(sender) = &self.sender
            && sender.send(Message::Flush(reply)).is_ok()
        {
            let _ = done.recv_timeout(Duration::from_secs(5));
        }
    }
}

impl Drop for SessionWriter {
    fn drop(&mut self) {
        // Closing the channel makes the thread write what's pending and stop.
        self.sender = None;
        if let Ok(mut writer) = WRITER.lock() {
            *writer = None;
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Write the pending session now, from any thread, waiting at most `timeout` (the shutdown
/// watchdog uses this, so the last changes are saved even if the main thread is stuck).
pub(crate) fn flush_from_any_thread(timeout: Duration) {
    let Some(sender) = WRITER.lock().ok().and_then(|writer| writer.clone()) else {
        return;
    };
    let (reply, done) = mpsc::channel();
    if sender.send(Message::Flush(reply)).is_ok() {
        let _ = done.recv_timeout(timeout);
    }
}

fn write_loop(path: &Path, receiver: Receiver<Message>) {
    let mut pending: Option<String> = None;
    let mut due: Option<Instant> = None;
    loop {
        let message = match due {
            Some(due) => receiver.recv_timeout(due.saturating_duration_since(Instant::now())),
            None => receiver.recv().map_err(|_| RecvTimeoutError::Disconnected),
        };
        match message {
            Ok(Message::Save(session)) => {
                pending = Some(session);
                // Debounce, but never put off a write for longer than WRITE_DELAY.
                due.get_or_insert_with(|| Instant::now() + WRITE_DELAY);
            },
            Ok(Message::Flush(reply)) => {
                if let Some(session) = pending.take() {
                    write_atomically(path, &session);
                }
                due = None;
                let _ = reply.send(());
            },
            Err(RecvTimeoutError::Timeout) => {
                if let Some(session) = pending.take() {
                    write_atomically(path, &session);
                }
                due = None;
            },
            Err(RecvTimeoutError::Disconnected) => {
                if let Some(session) = pending.take() {
                    write_atomically(path, &session);
                }
                return;
            },
        }
    }
}

/// Write to a temporary file, then rename it over the old one (an atomic replace on NTFS).
fn write_atomically(path: &Path, data: &str) {
    let temporary = path.with_extension("json.tmp");
    let result = path
        .parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .and_then(|_| std::fs::write(&temporary, data))
        .and_then(|_| std::fs::rename(&temporary, path));
    if let Err(error) = result {
        warn!("Could not save the session to {}: {error}", path.display());
    }
}
