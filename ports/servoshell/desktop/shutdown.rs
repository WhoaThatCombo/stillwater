/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Quitting can never leave a process behind. Once the user has closed the last window (or
//! asked to quit), a watchdog thread ends the process if it is still alive after
//! `DEADLINE`, whatever the main thread or the engine is stuck on. Without it a hang during
//! shutdown left an invisible process that kept the single-instance pipe, so every later
//! launch handed its URL to it and "Stillwater wouldn't start".
//!
//! What the watchdog could cut off:
//! - Tabs (session.json): written by `SessionWriter`'s thread at most a second after each
//!   change; the watchdog flushes it first.
//! - History, bookmarks, passwords, settings, spaces: written as they change.
//! - Cookies, HSTS and HTTP auth: written by the engine's resource thread as it stops, which
//!   normally takes well under a second after the close. `DEADLINE` leaves it many times that.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::desktop::{diagnostics, session};

/// How long after the close the process may still be running.
const DEADLINE: Duration = Duration::from_secs(6);

static REQUESTED: AtomicBool = AtomicBool::new(false);

/// Whether the user has asked this instance to quit. A quitting instance no longer accepts
/// URLs from later launches (see `single_instance`): they start a fresh one instead.
pub(crate) fn requested() -> bool {
    REQUESTED.load(Ordering::Acquire)
}

/// The user asked to quit (`reason` is for the log). Starts the watchdog; later calls do
/// nothing. Cheap and never blocks, so it is safe to call from the close handler before
/// anything that might hang.
pub(crate) fn begin(reason: &str) {
    if REQUESTED.swap(true, Ordering::AcqRel) {
        return;
    }
    let started = Instant::now();
    diagnostics::log(format!(
        "shutdown: {reason}; watchdog armed ({} s)",
        DEADLINE.as_secs()
    ));
    let spawned = std::thread::Builder::new()
        .name("ShutdownWatchdog".into())
        .spawn(move || {
            // Make sure the last tab changes are on disk, even if the main thread never gets
            // to drop the session writer.
            session::flush_from_any_thread(Duration::from_secs(2));
            std::thread::sleep(DEADLINE.saturating_sub(started.elapsed()));
            diagnostics::log_without_waiting(format!(
                "shutdown watchdog: still running {:.1} s after the close was requested; \
                 ending the process",
                started.elapsed().as_secs_f64()
            ));
            force_exit();
        });
    if spawned.is_err() {
        diagnostics::log("shutdown: could not start the watchdog");
    }
}

/// End the process now, after a normal shutdown has finished saving (see `cli::main`).
pub(crate) fn exit_now() -> ! {
    force_exit()
}

/// End the process now. On Windows this is `TerminateProcess`, not `ExitProcess`
/// (`std::process::exit`): `ExitProcess` runs DLL detach code and takes the loader lock,
/// which a stuck thread may hold, so it can hang in exactly the situation this is for.
fn force_exit() -> ! {
    #[cfg(target_os = "windows")]
    {
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetCurrentProcess() -> isize;
            fn TerminateProcess(process: isize, exit_code: u32) -> i32;
        }
        unsafe { TerminateProcess(GetCurrentProcess(), 0) };
    }
    std::process::exit(0)
}
