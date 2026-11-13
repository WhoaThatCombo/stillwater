/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! A small always-on diagnostic log for bug reports:
//! `%LOCALAPPDATA%\Stillwater\stillwater.log` (recreated on every launch, capped in size).
//! It records startup timing, once-a-second event-loop summaries and how mouse clicks
//! were routed, so a "frozen window" report comes with data.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::Write;
use std::sync::Mutex;
use std::time::Instant;

const MAX_BYTES: u64 = 4 * 1024 * 1024;

struct Log {
    file: File,
    written: u64,
    start: Instant,
    counters: BTreeMap<&'static str, u64>,
    last_summary: Instant,
    /// Since the last summary: milliseconds between presents that showed a new frame of the
    /// active page (its frame pacing), and the UI thread's time for every window repaint.
    frame_intervals: Vec<f32>,
    repaint_work: Vec<f32>,
    last_new_frame_present: Option<Instant>,
    /// When the active tab last changed, until its first frame is presented; and the
    /// time from each switch to that present.
    switch_started: Option<Instant>,
    switch_shown: Vec<f32>,
}

/// p50/p95/max of `values` in milliseconds, for the summary line.
fn percentiles(values: &mut [f32]) -> String {
    values.sort_by(f32::total_cmp);
    let at = |q: f32| values[((values.len() - 1) as f32 * q).round() as usize];
    format!("{:.1}/{:.1}/{:.1}", at(0.5), at(0.95), values[values.len() - 1])
}

static LOG: Mutex<Option<Log>> = Mutex::new(None);

/// Open the log. Safe to call once at startup; later calls are ignored.
pub(crate) fn init() {
    let Some(path) = log_path() else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let Ok(file) = File::create(path) else {
        return;
    };
    let mut guard = LOG.lock().unwrap_or_else(|poison| poison.into_inner());
    if guard.is_none() {
        *guard = Some(Log {
            file,
            written: 0,
            start: Instant::now(),
            counters: BTreeMap::new(),
            last_summary: Instant::now(),
            frame_intervals: Vec::new(),
            repaint_work: Vec::new(),
            last_new_frame_present: None,
            switch_started: None,
            switch_shown: Vec::new(),
        });
    }
}

/// Where the log goes. STILLWATER_LOG_FILE lets test copies log elsewhere instead of
/// overwriting the user's log.
fn log_path() -> Option<std::path::PathBuf> {
    let path = std::env::var_os("STILLWATER_LOG_FILE")
        .map(std::path::PathBuf::from)
        .or_else(|| dirs::data_local_dir().map(|dir| dir.join("Stillwater").join("stillwater.log")));
    // An incognito session has a log of its own (see `incognito::loggable` for what goes in it).
    if crate::desktop::incognito::active() {
        return path.map(|path| path.with_file_name(crate::desktop::incognito::log_file_name()));
    }
    path
}

/// The folder holding the log, for other files meant for bug reports.
pub(crate) fn dir() -> Option<std::path::PathBuf> {
    log_path().and_then(|path| path.parent().map(std::path::Path::to_path_buf))
}

/// Write one line, prefixed with seconds since launch.
pub(crate) fn log(message: impl AsRef<str>) {
    let mut guard = LOG.lock().unwrap_or_else(|poison| poison.into_inner());
    if let Some(log) = guard.as_mut() {
        write_line(log, message.as_ref());
    }
}

fn write_line(log: &mut Log, message: &str) {
    if log.written > MAX_BYTES ||
        (crate::desktop::incognito::active() && !crate::desktop::incognito::loggable(message))
    {
        return;
    }
    let line = format!("{:9.3} {}\n", log.start.elapsed().as_secs_f64(), message);
    log.written += line.len() as u64;
    let _ = log.file.write_all(line.as_bytes());
}

/// Like `log`, but gives up after half a second if another thread holds the log (for the
/// shutdown watchdog, which must not hang on a thread that is itself stuck).
pub(crate) fn log_without_waiting(message: impl AsRef<str>) {
    for _ in 0..50 {
        match LOG.try_lock() {
            Ok(mut guard) => {
                if let Some(log) = guard.as_mut() {
                    write_line(log, message.as_ref());
                }
                return;
            },
            Err(std::sync::TryLockError::Poisoned(poison)) => {
                if let Some(log) = poison.into_inner().as_mut() {
                    write_line(log, message.as_ref());
                }
                return;
            },
            Err(std::sync::TryLockError::WouldBlock) => {
                std::thread::sleep(std::time::Duration::from_millis(10))
            },
        }
    }
}

/// Count an occurrence of `name`; counts are written in the once-a-second summary.
pub(crate) fn count(name: &'static str) {
    let mut guard = LOG.lock().unwrap_or_else(|poison| poison.into_inner());
    if let Some(log) = guard.as_mut() {
        *log.counters.entry(name).or_default() += 1;
    }
}

/// Whether `STILLWATER_FRAME_LOG=1`: log every new frame of the active page as it becomes ready
/// (`ready`) and as it reaches the window (`present`), so frame pacing (video smoothness) can
/// be measured from the log.
fn frame_log() -> bool {
    static EACH: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *EACH.get_or_init(|| std::env::var_os("STILLWATER_FRAME_LOG").is_some_and(|v| v == "1"))
}

/// A new frame of the active page has been painted, and is shown with the window's next present.
static FRAME_PAINTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The active page has a new frame to show. With `STILLWATER_FRAME_LOG=1` the line carries the
/// page's vertical scroll offset at that moment (`ready y=<page px>`), so wheel-to-scrolled-frame
/// latency and smooth-scroll positions over time can be read from the log.
pub(crate) fn frame_ready(scroll_y: impl FnOnce() -> Option<f32>) {
    if frame_log() {
        match scroll_y() {
            Some(y) => log(format!("ready y={y:.1}")),
            None => log("ready"),
        }
    }
}

/// The window's active tab changed: the next new page frame presented is its first
/// (`switch_ms=` in the summary: tab switch to page shown).
pub(crate) fn tab_switched() {
    let mut guard = LOG.lock().unwrap_or_else(|poison| poison.into_inner());
    if let Some(log) = guard.as_mut() {
        log.switch_started = Some(Instant::now());
        *log.counters.entry("switch").or_default() += 1;
        if frame_log() {
            write_line(log, "switch: active changed");
        }
    }
}

/// The active page's new frame has been painted into the window's buffer.
pub(crate) fn frame_painted() {
    FRAME_PAINTED.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// The window was presented, `update` and `paint` after the start of its frame. Counts a
/// `present` (in the summary) if it showed a new frame of the active page.
pub(crate) fn window_presented(update: std::time::Duration, paint: std::time::Duration) {
    let new_frame = FRAME_PAINTED.swap(false, std::sync::atomic::Ordering::Relaxed);
    let mut guard = LOG.lock().unwrap_or_else(|poison| poison.into_inner());
    let Some(log) = guard.as_mut() else {
        return;
    };
    // Every repaint's cost on the UI thread (GUI update + WebRender passes + present), and the
    // spacing of new page frames, summarised once a second (see `maybe_summarize`): on a
    // 164 Hz monitor smooth animation shows as "frames_ms=6.1/6.6/..." (p50/p95/max), and
    // "repaint_ms" includes waiting for vsync in the present, so up to ~6 ms is normal.
    log.repaint_work.push(paint.as_secs_f32() * 1000.0);
    if !new_frame {
        return;
    }
    let now = Instant::now();
    if let Some(started) = log.switch_started.take() {
        let shown = now.duration_since(started).as_secs_f32() * 1000.0;
        log.switch_shown.push(shown);
        if frame_log() {
            write_line(log, &format!("switch: shown after {shown:.1} ms"));
        }
    }
    if let Some(last) = log.last_new_frame_present.replace(now) {
        let interval = now.duration_since(last).as_secs_f32() * 1000.0;
        // A gap of more than a quarter second is a pause between animations, not a frame.
        if interval < 250.0 {
            log.frame_intervals.push(interval);
        }
    }
    {
        *log.counters.entry("present").or_default() += 1;
        if frame_log() {
            let line = format!(
                "present {:.1} {:.1}",
                update.as_secs_f64() * 1000.0,
                paint.as_secs_f64() * 1000.0
            );
            write_line(log, &line);
        }
    }
}

/// Write the counters gathered since the last summary (at most once a second).
pub(crate) fn maybe_summarize(extra: impl FnOnce() -> String) {
    let summary = {
        let mut guard = LOG.lock().unwrap_or_else(|poison| poison.into_inner());
        let Some(log) = guard.as_mut() else {
            return;
        };
        if log.last_summary.elapsed().as_secs_f64() < 1.0 ||
            (log.counters.is_empty() && log.repaint_work.is_empty())
        {
            return;
        }
        log.last_summary = Instant::now();
        let counters: Vec<String> = log
            .counters
            .iter()
            .map(|(name, count)| format!("{name}={count}"))
            .collect();
        log.counters.clear();
        let mut summary = counters.join(" ");
        if !log.frame_intervals.is_empty() {
            summary += &format!(" frames_ms={}", percentiles(&mut log.frame_intervals));
            log.frame_intervals.clear();
        }
        if !log.repaint_work.is_empty() {
            summary += &format!(" repaint_ms={}", percentiles(&mut log.repaint_work));
            log.repaint_work.clear();
        }
        if !log.switch_shown.is_empty() {
            summary += &format!(" switch_ms={}", percentiles(&mut log.switch_shown));
            log.switch_shown.clear();
        }
        summary
    };
    log(format!("summary: {summary} | {}", extra()));
}

