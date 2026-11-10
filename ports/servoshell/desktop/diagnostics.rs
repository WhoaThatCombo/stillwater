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
}

static LOG: Mutex<Option<Log>> = Mutex::new(None);

/// Open the log. Safe to call once at startup; later calls are ignored.
pub(crate) fn init() {
    let Some(dir) = dirs::data_local_dir().map(|dir| dir.join("Stillwater")) else {
        return;
    };
    let _ = std::fs::create_dir_all(&dir);
    // STILLWATER_LOG_FILE lets test copies log elsewhere instead of overwriting the user's log.
    let path = std::env::var_os("STILLWATER_LOG_FILE")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| dir.join("stillwater.log"));
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
        });
    }
}

/// Write one line, prefixed with seconds since launch.
pub(crate) fn log(message: impl AsRef<str>) {
    let mut guard = LOG.lock().unwrap_or_else(|poison| poison.into_inner());
    if let Some(log) = guard.as_mut() {
        write_line(log, message.as_ref());
    }
}

fn write_line(log: &mut Log, message: &str) {
    if log.written > MAX_BYTES {
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

/// Write the counters gathered since the last summary (at most once a second).
pub(crate) fn maybe_summarize(extra: impl FnOnce() -> String) {
    let summary = {
        let mut guard = LOG.lock().unwrap_or_else(|poison| poison.into_inner());
        let Some(log) = guard.as_mut() else {
            return;
        };
        if log.last_summary.elapsed().as_secs_f64() < 1.0 || log.counters.is_empty() {
            return;
        }
        log.last_summary = Instant::now();
        let counters: Vec<String> = log
            .counters
            .iter()
            .map(|(name, count)| format!("{name}={count}"))
            .collect();
        log.counters.clear();
        counters.join(" ")
    };
    log(format!("summary: {summary} | {}", extra()));
}

