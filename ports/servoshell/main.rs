/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! The `servoshell` test application.
//!
//! Creates a `Servo` instance with a example implementation of a working
//! web browser.
//!
//! This browser's implementation of `WindowMethods` is built on top
//! of [winit], the cross-platform windowing library.
//!
//! For the engine itself look next door in `components/servo/lib.rs`.
//!
//! [winit]: https://github.com/rust-windowing/winit

// A windowed program on Windows: a console program gets a console window that pops up (and
// takes focus) for a moment on every launch, even though it's freed straight away.
#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

#[cfg(target_os = "windows")]
use windows_sys::Win32::System::Console;

fn main() {
    #[cfg(target_os = "windows")]
    // SAFETY: No safety related side effects or requirements.
    unsafe {
        // Started from a terminal: print to it. Leave output that's redirected (to a file or
        // a pipe, e.g. by tests) alone. Otherwise there's no console, which is fine.
        let output = Console::GetStdHandle(Console::STD_OUTPUT_HANDLE);
        if output.is_null() || output == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
            let _result = Console::AttachConsole(Console::ATTACH_PARENT_PROCESS);
        }
    }
    // A background test window is also silent (the media backend reads STILLWATER_MUTE_AUDIO).
    // Set before any thread starts, while changing the environment is still safe.
    if std::env::var_os("STILLWATER_BACKGROUND_WINDOW").is_some() {
        // SAFETY: no other threads exist yet.
        unsafe { std::env::set_var("STILLWATER_MUTE_AUDIO", "1") };
    }
    cfg_if::cfg_if! {
        if #[cfg(not(any(target_os = "android", target_env = "ohos")))] {
            servoshell::main()
        } else {
            // Android: see ports/servoshell/egl/android/mod.rs.
            // OpenHarmony: see ports/servoshell/egl/ohos/mod.rs.
            println!(
                "Cannot run the servoshell `bin` executable on platforms such as \
                 Android or OpenHarmony. On these platforms you need to compile \
                 the servoshell library as a `cdylib` and integrate it with the \
                 platform app code into an `apk` (android) or `hap` (OpenHarmony).\
                 For Android `mach build` will do these steps automatically for you."
            );
        }
    }
}
