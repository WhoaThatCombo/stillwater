/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! A [`RefreshDriver`] that starts animation frames on the display's vertical blank (Windows).
//!
//! Without one, Paint starts animation frames (requestAnimationFrame, CSS animations, the
//! page frames of a site that animates while a video plays) from an 8 ms timer armed when the
//! previous frame is painted: the period is 8 ms plus however long the frame took to make, it
//! has nothing to do with the monitor, and frames beat against the display (on a 164 Hz screen,
//! a 70-120 Hz cadence shows as uneven 6/12 ms steps). Waiting for the vertical blank of the
//! monitor the window is on, like Chromium's VSyncThreadWin, starts each frame right after a
//! refresh, at the monitor's own rate, and costs nothing while nobody asks for frames.
//!
//! D3DKMTWaitForVerticalBlankEvent is used rather than DwmFlush: DwmFlush returns at once when
//! the desktop compositor has nothing to compose (exactly when our own frame was late), which
//! made the earlier DwmFlush version fall back to a 60 Hz timer on every missed frame.

use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use servo::RefreshDriver;
use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::Graphics::Gdi::{
    CreateDCW, DeleteDC, GetMonitorInfoW, HDC, HMONITOR, MONITOR_DEFAULTTONEAREST, MONITORINFOEXW,
    MonitorFromWindow,
};

type FrameCallback = Box<dyn Fn() + Send + 'static>;

#[repr(C)]
#[allow(non_snake_case)]
struct D3dkmtOpenAdapterFromHdc {
    hDc: HDC,
    hAdapter: u32,
    AdapterLuidLow: u32,
    AdapterLuidHigh: i32,
    VidPnSourceId: u32,
}

#[repr(C)]
#[allow(non_snake_case)]
struct D3dkmtWaitForVerticalBlankEvent {
    hAdapter: u32,
    hDevice: u32,
    VidPnSourceId: u32,
}

#[repr(C)]
#[allow(non_snake_case)]
struct D3dkmtCloseAdapter {
    hAdapter: u32,
}

#[link(name = "gdi32")]
unsafe extern "system" {
    fn D3DKMTOpenAdapterFromHdc(args: *mut D3dkmtOpenAdapterFromHdc) -> i32;
    fn D3DKMTWaitForVerticalBlankEvent(args: *const D3dkmtWaitForVerticalBlankEvent) -> i32;
    fn D3DKMTCloseAdapter(args: *const D3dkmtCloseAdapter) -> i32;
}

#[derive(Default)]
struct State {
    callbacks: Vec<FrameCallback>,
    quit: bool,
}

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    wake: Condvar,
}

pub(crate) struct VsyncRefreshDriver {
    shared: Arc<Shared>,
}

impl VsyncRefreshDriver {
    /// `hwnd`: the window whose monitor paces the frames (looked up again on every frame, so
    /// moving the window to another monitor follows that monitor's rate).
    pub(crate) fn new(hwnd: isize) -> Self {
        let shared = Arc::new(Shared::default());
        let thread_shared = shared.clone();
        let _ = std::thread::Builder::new()
            .name("VsyncRefresh".into())
            .spawn(move || run(&thread_shared, hwnd));
        Self { shared }
    }
}

impl RefreshDriver for VsyncRefreshDriver {
    fn observe_next_frame(&self, start_frame_callback: FrameCallback) {
        let mut state = self.shared.state.lock().unwrap_or_else(|poison| poison.into_inner());
        state.callbacks.push(start_frame_callback);
        self.shared.wake.notify_one();
    }
}

impl Drop for VsyncRefreshDriver {
    fn drop(&mut self) {
        let mut state = self.shared.state.lock().unwrap_or_else(|poison| poison.into_inner());
        state.quit = true;
        self.shared.wake.notify_one();
    }
}

/// The adapter and output (VidPn source) driving one monitor.
struct Output {
    monitor: HMONITOR,
    adapter: u32,
    source: u32,
}

impl Output {
    fn open(monitor: HMONITOR) -> Option<Self> {
        // SAFETY: plain Win32 calls on a zeroed, correctly sized MONITORINFOEXW; the DC is
        // deleted before returning.
        unsafe {
            let mut info: MONITORINFOEXW = std::mem::zeroed();
            info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
            if GetMonitorInfoW(monitor, &mut info as *mut MONITORINFOEXW as *mut _) == 0 {
                return None;
            }
            let dc = CreateDCW(
                std::ptr::null(),
                info.szDevice.as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
            );
            if dc.is_null() {
                return None;
            }
            let mut open = D3dkmtOpenAdapterFromHdc {
                hDc: dc,
                hAdapter: 0,
                AdapterLuidLow: 0,
                AdapterLuidHigh: 0,
                VidPnSourceId: 0,
            };
            let status = D3DKMTOpenAdapterFromHdc(&mut open);
            DeleteDC(dc);
            (status >= 0).then_some(Output {
                monitor,
                adapter: open.hAdapter,
                source: open.VidPnSourceId,
            })
        }
    }

    fn wait(&self) -> bool {
        let wait = D3dkmtWaitForVerticalBlankEvent {
            hAdapter: self.adapter,
            hDevice: 0,
            VidPnSourceId: self.source,
        };
        // SAFETY: blocks this thread until the next vertical blank of an adapter we opened.
        unsafe { D3DKMTWaitForVerticalBlankEvent(&wait) >= 0 }
    }
}

impl Drop for Output {
    fn drop(&mut self) {
        // SAFETY: closes the adapter handle opened in `Output::open`.
        unsafe {
            D3DKMTCloseAdapter(&D3dkmtCloseAdapter {
                hAdapter: self.adapter,
            });
        }
    }
}

fn run(shared: &Shared, hwnd: isize) {
    let mut output: Option<Output> = None;
    let mut last_frame = Instant::now();
    loop {
        // Sleep (no wake-ups at all) until someone wants a frame.
        {
            let mut state = shared.state.lock().unwrap_or_else(|poison| poison.into_inner());
            while state.callbacks.is_empty() && !state.quit {
                state = shared.wake.wait(state).unwrap_or_else(|poison| poison.into_inner());
            }
            if state.quit {
                return;
            }
        }

        // SAFETY: MonitorFromWindow accepts any window handle, from any thread.
        let monitor = unsafe { MonitorFromWindow(hwnd as HWND, MONITOR_DEFAULTTONEAREST) };
        if output.as_ref().is_none_or(|output| output.monitor != monitor) {
            output = Output::open(monitor);
        }
        let waited = output.as_ref().is_some_and(Output::wait);
        // No vblank to wait for (remote desktop, a driver without the call, the monitor went
        // away): pace at 60 Hz rather than spin. A "vblank" much sooner than any display
        // refreshes also means we aren't being paced.
        if !waited || last_frame.elapsed() < Duration::from_micros(1500) {
            output = None;
            std::thread::sleep(Duration::from_micros(16_667).saturating_sub(last_frame.elapsed()));
        }
        last_frame = Instant::now();

        let callbacks = std::mem::take(
            &mut shared.state.lock().unwrap_or_else(|poison| poison.into_inner()).callbacks,
        );
        for callback in callbacks {
            callback();
        }
    }
}
