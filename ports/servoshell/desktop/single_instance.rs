/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Keep one Stillwater process per user, like a normal browser: when Windows launches
//! `Stillwater.exe <url>` (for example because Stillwater is the default browser and the
//! user clicked a link elsewhere), hand the URL to the running instance over a named pipe
//! and exit, instead of starting a second browser.
//!
//! A launch only exits once the running instance has *acknowledged* the URL, and the
//! acknowledgement comes from that instance's event loop handling it, not from its pipe
//! thread: a copy whose event loop is stuck (say, hung while quitting) has a live pipe
//! thread, and used to swallow every later launch so "Stillwater wouldn't start". Without an
//! acknowledgement within a couple of seconds the launch starts its own window.
//!
//! So that a stuck copy can't keep the pipe name, each instance listens on its own
//! generation of the name, `\\.\pipe\stillwater2-<user>-<n>`, a new instance taking the
//! number after the highest one in use. A launch tries the highest (newest) first.
//!
//! Stillwater 0.8.5 and older listen on `\\.\pipe\stillwater-<user>` and never acknowledge.
//! If only such a copy is running, the URL goes to it when it has a visible window that
//! answers window messages (it is alive); otherwise the launch starts its own window.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use winit::event_loop::EventLoopProxy;

use crate::desktop::event_loop::AppEvent;

const PENDING: u8 = 0;
const HANDLED: u8 = 1;
const ABANDONED: u8 = 2;

/// One URL handed over by a later launch, shared between the pipe thread (which answers the
/// launch) and the event loop (which opens the URL).
#[derive(Clone, Debug, Default)]
pub(crate) struct Handoff(Arc<AtomicU8>);

impl Handoff {
    /// Called by the event loop as it handles the URL. False if the pipe thread already gave
    /// up waiting and told the launch to open its own window (the URL must not open here too).
    pub(crate) fn claim(&self) -> bool {
        self.0
            .compare_exchange(PENDING, HANDLED, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    #[cfg_attr(not(target_os = "windows"), expect(dead_code))]
    fn handled(&self) -> bool {
        self.0.load(Ordering::Acquire) == HANDLED
    }

    /// Give up waiting. False if the event loop claimed the URL first.
    #[cfg_attr(not(target_os = "windows"), expect(dead_code))]
    fn abandon(&self) -> bool {
        self.0
            .compare_exchange(PENDING, ABANDONED, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }
}

#[cfg(target_os = "windows")]
mod imp {
    use std::ptr::{null, null_mut};
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_IO_PENDING, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, GENERIC_READ,
        GENERIC_WRITE, GetLastError, HANDLE, HWND, INVALID_HANDLE_VALUE, LPARAM,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, FILE_FLAGS_AND_ATTRIBUTES,
        FindClose, FindFirstFileW, FindNextFileW, FlushFileBuffers, OPEN_EXISTING,
        PIPE_ACCESS_DUPLEX, ReadFile, SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT,
        WIN32_FIND_DATAW, WriteFile,
    };
    use windows_sys::Win32::System::IO::{
        CancelIoEx, GetOverlappedResult, GetOverlappedResultEx, OVERLAPPED,
    };
    use windows_sys::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, GetNamedPipeServerProcessId,
        PIPE_READMODE_BYTE, PeekNamedPipe, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES,
        PIPE_WAIT, WaitNamedPipeW,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        ASFW_ANY, AllowSetForegroundWindow, EnumWindows, GetWindowTextLengthW,
        GetWindowThreadProcessId, IsWindowVisible, SMTO_ABORTIFHUNG, SMTO_BLOCK,
        SendMessageTimeoutW, WM_NULL,
    };
    use winit::event_loop::EventLoopProxy;

    use super::Handoff;
    use crate::desktop::event_loop::AppEvent;
    use crate::desktop::{diagnostics, shutdown};

    /// The most we read from one later launch. A URL is far shorter; this stops another program
    /// on the machine from making us buffer without limit.
    const MAX_MESSAGE: usize = 8 * 1024;
    /// How long the pipe thread waits for the event loop to take a URL.
    const ACK_TIMEOUT: Duration = Duration::from_secs(2);
    /// How long a launch waits for an answer from one running instance (a little longer than
    /// `ACK_TIMEOUT`, so a live instance's "no" arrives before the launch gives up).
    const REPLY_TIMEOUT: Duration = Duration::from_millis(2500);
    /// Pipe generations in use at once (stuck copies plus the live one).
    const GENERATIONS: u32 = 16;

    /// What happened on the way, for the log (which only opens after the hand-off).
    static NOTES: Mutex<Vec<String>> = Mutex::new(Vec::new());

    fn note(message: String) {
        if let Ok(mut notes) = NOTES.lock() {
            notes.push(message);
        }
    }

    pub(super) fn take_notes() -> Vec<String> {
        NOTES.lock().map(|mut notes| std::mem::take(&mut *notes)).unwrap_or_default()
    }

    fn user() -> String {
        std::env::var("USERNAME").unwrap_or_default()
    }

    fn wide(name: &str) -> Vec<u16> {
        name.encode_utf16().chain(Some(0)).collect()
    }

    fn pipe_prefix() -> String {
        format!("stillwater2-{}-", user())
    }

    fn pipe_name(generation: u32) -> Vec<u16> {
        wide(&format!(r"\\.\pipe\{}{generation}", pipe_prefix()))
    }

    /// The name Stillwater 0.8.5 and older listen on.
    fn legacy_pipe_name() -> Vec<u16> {
        wide(&format!(r"\\.\pipe\stillwater-{}", user()))
    }

    /// The generations some instance is listening on, lowest first.
    fn generations_in_use() -> Vec<u32> {
        let prefix = pipe_prefix();
        let mut generations = Vec::new();
        let pattern = wide(r"\\.\pipe\*");
        let mut data: WIN32_FIND_DATAW = unsafe { std::mem::zeroed() };
        let find = unsafe { FindFirstFileW(pattern.as_ptr(), &mut data) };
        if find == INVALID_HANDLE_VALUE {
            return generations;
        }
        loop {
            let length = data.cFileName.iter().position(|c| *c == 0).unwrap_or(data.cFileName.len());
            let name = String::from_utf16_lossy(&data.cFileName[..length]);
            if let Some(generation) = name
                .strip_prefix(&prefix)
                .and_then(|rest| rest.parse::<u32>().ok())
                .filter(|generation| *generation < GENERATIONS)
            {
                generations.push(generation);
            }
            if unsafe { FindNextFileW(find, &mut data) } == 0 {
                break;
            }
        }
        unsafe { FindClose(find) };
        generations.sort_unstable();
        generations.dedup();
        generations
    }

    /// Connect to a running instance's pipe, waiting a moment if it is busy with another launch.
    fn connect(name: &[u16], access: u32, flags: FILE_FLAGS_AND_ATTRIBUTES) -> Option<HANDLE> {
        for _ in 0..2 {
            let pipe = unsafe {
                CreateFileW(
                    name.as_ptr(),
                    access,
                    0,
                    null(),
                    OPEN_EXISTING,
                    // Only let the other end identify us, never act as us, in case something
                    // other than Stillwater took the pipe name first.
                    flags | SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
                    null_mut(),
                )
            };
            if pipe != INVALID_HANDLE_VALUE {
                return Some(pipe);
            }
            if unsafe { GetLastError() } != ERROR_PIPE_BUSY ||
                unsafe { WaitNamedPipeW(name.as_ptr(), 1000) } == 0
            {
                return None;
            }
        }
        None
    }

    /// Run one overlapped read or write on `pipe`, giving up at `deadline`. Returns the bytes
    /// transferred, or `None` on failure or timeout.
    fn overlapped(
        pipe: HANDLE,
        deadline: Instant,
        start: impl FnOnce(*mut OVERLAPPED) -> i32,
    ) -> Option<u32> {
        let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
        if start(&mut overlapped) == 0 && unsafe { GetLastError() } != ERROR_IO_PENDING {
            return None;
        }
        let wait = deadline.saturating_duration_since(Instant::now()).as_millis() as u32;
        let mut transferred = 0;
        if unsafe { GetOverlappedResultEx(pipe, &overlapped, &mut transferred, wait, 0) } != 0 {
            return Some(transferred);
        }
        // Timed out (or failed): cancel, and wait for the cancellation so `overlapped` can go.
        unsafe {
            CancelIoEx(pipe, &overlapped);
            GetOverlappedResult(pipe, &overlapped, &mut transferred, 1);
        }
        None
    }

    /// Hand `url` to the instance listening on `generation`. Ok once it has taken the URL.
    fn send(generation: u32, url: &str) -> Result<(), String> {
        let name = pipe_name(generation);
        let Some(pipe) = connect(&name, GENERIC_READ | GENERIC_WRITE, FILE_FLAG_OVERLAPPED) else {
            return Err(format!("generation {generation}: could not connect"));
        };
        // We're the foreground process (the user just launched us), so let the running
        // instance bring its window to the front.
        unsafe { AllowSetForegroundWindow(ASFW_ANY) };
        let deadline = Instant::now() + REPLY_TIMEOUT;
        let message = format!("{}\n", url.replace(['\r', '\n'], ""));
        let bytes = message.as_bytes();
        let written = overlapped(pipe, deadline, |o| unsafe {
            WriteFile(pipe, bytes.as_ptr(), bytes.len() as u32, null_mut(), o)
        });
        let mut reply = Vec::new();
        if written == Some(bytes.len() as u32) {
            let mut buffer = [0u8; 64];
            while !reply.contains(&b'\n') && reply.len() < 256 {
                match overlapped(pipe, deadline, |o| unsafe {
                    ReadFile(pipe, buffer.as_mut_ptr(), buffer.len() as u32, null_mut(), o)
                }) {
                    Some(read) if read > 0 => reply.extend_from_slice(&buffer[..read as usize]),
                    _ => break,
                }
            }
        }
        unsafe { CloseHandle(pipe) };
        match String::from_utf8_lossy(&reply).lines().next() {
            Some("OK") => Ok(()),
            Some(answer) if !answer.is_empty() => {
                Err(format!("generation {generation}: answered {answer:?}"))
            },
            _ => Err(format!(
                "generation {generation}: no answer within {} ms",
                REPLY_TIMEOUT.as_millis()
            )),
        }
    }

    /// Whether process `pid` has a visible, titled window that answers window messages: a
    /// running Stillwater 0.8.5 or older that is alive, not one hung while quitting (its
    /// window is hidden at once on close).
    fn has_responsive_window(pid: u32) -> bool {
        struct Search {
            pid: u32,
            windows: Vec<HWND>,
        }
        unsafe extern "system" fn visit(window: HWND, search: LPARAM) -> i32 {
            let search = unsafe { &mut *(search as *mut Search) };
            let mut owner = 0;
            unsafe { GetWindowThreadProcessId(window, &mut owner) };
            if owner == search.pid &&
                unsafe { IsWindowVisible(window) } != 0 &&
                unsafe { GetWindowTextLengthW(window) } > 0
            {
                search.windows.push(window);
            }
            1
        }
        let mut search = Search {
            pid,
            windows: Vec::new(),
        };
        unsafe { EnumWindows(Some(visit), &mut search as *mut Search as LPARAM) };
        search.windows.into_iter().any(|window| {
            let mut result = 0;
            let answered = unsafe {
                SendMessageTimeoutW(
                    window,
                    WM_NULL,
                    0,
                    0,
                    SMTO_ABORTIFHUNG | SMTO_BLOCK,
                    2000,
                    &mut result,
                )
            };
            answered != 0
        })
    }

    /// Hand `url` to a Stillwater 0.8.5 or older, which can't acknowledge it: only if it looks
    /// alive.
    fn send_legacy(url: &str) -> bool {
        let Some(pipe) = connect(&legacy_pipe_name(), GENERIC_WRITE, 0) else {
            return false;
        };
        let mut pid = 0;
        unsafe { GetNamedPipeServerProcessId(pipe, &mut pid) };
        if pid == 0 || !has_responsive_window(pid) {
            // Nothing written: an old instance that is alive after all just comes forward.
            unsafe { CloseHandle(pipe) };
            note(format!(
                "an older Stillwater (process {pid}) holds the pipe but has no responsive window; starting a new instance"
            ));
            return false;
        }
        unsafe { AllowSetForegroundWindow(ASFW_ANY) };
        let bytes = url.as_bytes();
        let mut written = 0;
        unsafe {
            WriteFile(pipe, bytes.as_ptr(), bytes.len() as u32, &mut written, null_mut());
            CloseHandle(pipe);
        }
        true
    }

    pub(super) fn forward(url: &str) -> bool {
        // Newest first: an older generation still in use is most likely a stuck copy.
        for generation in generations_in_use().into_iter().rev() {
            match send(generation, url) {
                Ok(()) => return true,
                Err(problem) => note(format!("hand-off: {problem}")),
            }
        }
        send_legacy(url)
    }

    /// Create one listening end of the pipe. The very first one is created with
    /// `FILE_FLAG_FIRST_PIPE_INSTANCE`, so if another program (or instance) already owns the
    /// name we fail instead of quietly sharing it.
    fn create_pipe(name: &[u16], first: bool) -> Option<HANDLE> {
        let first_flag = if first { FILE_FLAG_FIRST_PIPE_INSTANCE } else { 0 };
        let pipe = unsafe {
            CreateNamedPipeW(
                name.as_ptr(),
                PIPE_ACCESS_DUPLEX | first_flag,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                PIPE_UNLIMITED_INSTANCES,
                4096,
                4096,
                0,
                null(),
            )
        };
        (pipe != INVALID_HANDLE_VALUE).then_some(pipe)
    }

    /// Read one line from a later launch, up to `MAX_MESSAGE` bytes (without the newline).
    fn read_message(pipe: HANDLE) -> Option<Vec<u8>> {
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
                return None;
            }
            message.extend_from_slice(&buffer[..read as usize]);
            if let Some(end) = message.iter().position(|byte| *byte == b'\n') {
                message.truncate(end);
                return Some(message);
            }
        }
        None
    }

    fn reply(pipe: HANDLE, answer: &[u8]) {
        let mut written = 0;
        unsafe {
            WriteFile(pipe, answer.as_ptr(), answer.len() as u32, &mut written, null_mut());
            // Let the launch read it before the pipe is disconnected.
            FlushFileBuffers(pipe);
        }
    }

    /// Answer one later launch. Returns false once the event loop is gone.
    fn handle_launch(pipe: HANDLE, proxy: &EventLoopProxy<AppEvent>) -> bool {
        let Some(message) = read_message(pipe) else {
            return true;
        };
        // The launch gave up already (this thread was held up, say the process was suspended):
        // it has opened its own window, so this one must not open the URL too.
        let mut available = 0;
        if unsafe { PeekNamedPipe(pipe, null_mut(), 0, null_mut(), &mut available, null_mut()) } == 0 {
            diagnostics::log("single instance: a launch's URL arrived after it gave up waiting; ignored");
            return true;
        }
        if shutdown::requested() {
            diagnostics::log("single instance: a new launch arrived while quitting; it starts its own window");
            reply(pipe, b"NO quitting\n");
            return true;
        }
        let url = String::from_utf8_lossy(&message).into_owned();
        let handoff = Handoff::default();
        if proxy
            .send_event(AppEvent::OpenUrl(url, Some(handoff.clone())))
            .is_err()
        {
            reply(pipe, b"NO stopped\n");
            return false;
        }
        let deadline = Instant::now() + ACK_TIMEOUT;
        while !handoff.handled() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        if handoff.handled() || !handoff.abandon() {
            reply(pipe, b"OK\n");
        } else {
            diagnostics::log(format!(
                "single instance: the event loop didn't take a URL within {} ms; the new launch starts its own window",
                ACK_TIMEOUT.as_millis()
            ));
            reply(pipe, b"NO busy\n");
        }
        true
    }

    pub(super) fn listen(proxy: EventLoopProxy<AppEvent>) {
        let _ = std::thread::Builder::new()
            .name("StillwaterSingleInstance".into())
            .spawn(move || {
                // The generation after the newest one in use, so launches find us first.
                let start = generations_in_use().last().map_or(0, |newest| newest + 1);
                let Some((generation, mut pipe)) = (start..GENERATIONS)
                    .chain(0..start.min(GENERATIONS))
                    .find_map(|generation| {
                        create_pipe(&pipe_name(generation), true).map(|pipe| (generation, pipe))
                    })
                else {
                    diagnostics::log("single instance: could not create the pipe");
                    return;
                };
                let name = pipe_name(generation);
                diagnostics::log(format!("single instance: listening (generation {generation})"));
                loop {
                    let connected = unsafe { ConnectNamedPipe(pipe, null_mut()) } != 0 ||
                        unsafe { GetLastError() } == ERROR_PIPE_CONNECTED;
                    // Start the next listening end before handling this one, so the name is
                    // never left without a Stillwater pipe for another program to take.
                    let next = create_pipe(&name, false);
                    let running = !connected || handle_launch(pipe, &proxy);
                    unsafe {
                        DisconnectNamedPipe(pipe);
                        CloseHandle(pipe);
                    }
                    let Some(next) = next.filter(|_| running) else {
                        if let Some(next) = next {
                            unsafe { CloseHandle(next) };
                        } else {
                            diagnostics::log("single instance: could not create the pipe");
                        }
                        return;
                    };
                    pipe = next;
                }
            });
    }
}

/// If another Stillwater is running and takes `url`, return true (the caller should exit).
pub(crate) fn forward_to_running_instance(url: &str) -> bool {
    #[cfg(target_os = "windows")]
    return imp::forward(url);
    #[cfg(not(target_os = "windows"))]
    {
        let _ = url;
        false
    }
}

/// What happened while trying to hand over the URL, to write to the log once it is open.
pub(crate) fn take_notes() -> Vec<String> {
    #[cfg(target_os = "windows")]
    return imp::take_notes();
    #[cfg(not(target_os = "windows"))]
    Vec::new()
}

/// Accept URLs from later launches and open them as tabs in this instance.
pub(crate) fn listen_for_other_instances(proxy: EventLoopProxy<AppEvent>) {
    #[cfg(target_os = "windows")]
    imp::listen(proxy);
    #[cfg(not(target_os = "windows"))]
    let _ = proxy;
}
