/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Keep one Stillwater process per user, like a normal browser: when Windows launches
//! `Stillwater.exe <url>` (for example because Stillwater is the default browser and the
//! user clicked a link elsewhere), hand the URL to the running instance over a named pipe
//! and exit, instead of starting a second browser.

use winit::event_loop::EventLoopProxy;

use crate::desktop::event_loop::AppEvent;

#[cfg(target_os = "windows")]
mod imp {
    use std::ptr::{null, null_mut};

    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, GENERIC_WRITE, GetLastError, HANDLE,
        INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_FIRST_PIPE_INSTANCE, OPEN_EXISTING, PIPE_ACCESS_INBOUND, ReadFile,
        SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT, WriteFile,
    };
    use windows_sys::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE,
        PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
        WaitNamedPipeW,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{ASFW_ANY, AllowSetForegroundWindow};
    use winit::event_loop::EventLoopProxy;

    use crate::desktop::event_loop::AppEvent;

    /// The most we read from one later launch. A URL is far shorter; this stops another program
    /// on the machine from making us buffer without limit.
    const MAX_MESSAGE: usize = 8 * 1024;

    fn pipe_name() -> Vec<u16> {
        let user = std::env::var("USERNAME").unwrap_or_default();
        format!(r"\\.\pipe\stillwater-{user}")
            .encode_utf16()
            .chain(Some(0))
            .collect()
    }

    pub(super) fn forward(url: &str) -> bool {
        let name = pipe_name();
        for _ in 0..2 {
            let pipe = unsafe {
                CreateFileW(
                    name.as_ptr(),
                    GENERIC_WRITE,
                    0,
                    null(),
                    OPEN_EXISTING,
                    // Only let the other end identify us, never act as us, in case something
                    // other than Stillwater took the pipe name first.
                    SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
                    null_mut(),
                )
            };
            if pipe == INVALID_HANDLE_VALUE {
                // Another launch is talking to the running instance right now; wait for it.
                if unsafe { GetLastError() } == ERROR_PIPE_BUSY &&
                    unsafe { WaitNamedPipeW(name.as_ptr(), 2000) } != 0
                {
                    continue;
                }
                return false;
            }
            // We're the foreground process (the user just launched us), so let the running
            // instance bring its window to the front.
            unsafe { AllowSetForegroundWindow(ASFW_ANY) };
            let bytes = url.as_bytes();
            let mut written = 0;
            unsafe {
                WriteFile(
                    pipe,
                    bytes.as_ptr(),
                    bytes.len() as u32,
                    &mut written,
                    null_mut(),
                );
                CloseHandle(pipe);
            }
            return true;
        }
        false
    }

    /// Create one listening end of the pipe. The very first one is created with
    /// `FILE_FLAG_FIRST_PIPE_INSTANCE`, so if another program already owns the name we fail
    /// instead of quietly sharing it.
    fn create_pipe(name: &[u16], first: bool) -> Option<HANDLE> {
        let first_flag = if first { FILE_FLAG_FIRST_PIPE_INSTANCE } else { 0 };
        let pipe = unsafe {
            CreateNamedPipeW(
                name.as_ptr(),
                PIPE_ACCESS_INBOUND | first_flag,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                PIPE_UNLIMITED_INSTANCES,
                0,
                4096,
                0,
                null(),
            )
        };
        (pipe != INVALID_HANDLE_VALUE).then_some(pipe)
    }

    /// Read what a later launch sent, up to `MAX_MESSAGE` bytes.
    fn read_message(pipe: HANDLE) -> Vec<u8> {
        let mut message = Vec::new();
        let mut buffer = [0u8; 4096];
        while message.len() < MAX_MESSAGE {
            let mut read = 0;
            let ok = unsafe {
                ReadFile(
                    pipe,
                    buffer.as_mut_ptr(),
                    buffer.len() as u32,
                    &mut read,
                    null_mut(),
                )
            };
            if ok == 0 || read == 0 {
                break;
            }
            message.extend_from_slice(&buffer[..read as usize]);
        }
        message.truncate(MAX_MESSAGE);
        message
    }

    pub(super) fn listen(proxy: EventLoopProxy<AppEvent>) {
        let name = pipe_name();
        let _ = std::thread::Builder::new()
            .name("StillwaterSingleInstance".into())
            .spawn(move || {
                let Some(mut pipe) = create_pipe(&name, true) else {
                    log::warn!("Could not create the single-instance pipe");
                    return;
                };
                loop {
                    let connected = unsafe { ConnectNamedPipe(pipe, null_mut()) } != 0 ||
                        unsafe { GetLastError() } == ERROR_PIPE_CONNECTED;
                    // Start the next listening end before handling this one, so the name is
                    // never left without a Stillwater pipe for another program to take.
                    let next = create_pipe(&name, false);
                    let url = connected
                        .then(|| String::from_utf8_lossy(&read_message(pipe)).into_owned());
                    unsafe {
                        DisconnectNamedPipe(pipe);
                        CloseHandle(pipe);
                    }
                    if let Some(url) = url &&
                        proxy.send_event(AppEvent::OpenUrl(url)).is_err()
                    {
                        if let Some(next) = next {
                            unsafe { CloseHandle(next) };
                        }
                        return;
                    }
                    let Some(next) = next else {
                        log::warn!("Could not create the single-instance pipe");
                        return;
                    };
                    pipe = next;
                }
            });
    }
}

/// If another Stillwater is running, send it `url` and return true (the caller should exit).
pub(crate) fn forward_to_running_instance(url: &str) -> bool {
    #[cfg(target_os = "windows")]
    return imp::forward(url);
    #[cfg(not(target_os = "windows"))]
    {
        let _ = url;
        false
    }
}

/// Accept URLs from later launches and open them as tabs in this instance.
pub(crate) fn listen_for_other_instances(proxy: EventLoopProxy<AppEvent>) {
    #[cfg(target_os = "windows")]
    imp::listen(proxy);
    #[cfg(not(target_os = "windows"))]
    let _ = proxy;
}
