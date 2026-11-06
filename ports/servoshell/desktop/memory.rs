/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! System memory checks for the tab unloader.

/// Windows is running low on physical memory: less than 8% of RAM is available.
#[cfg(windows)]
pub(crate) fn system_memory_low() -> bool {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};

    let mut status: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    status.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
    if unsafe { GlobalMemoryStatusEx(&mut status) } == 0 {
        return false;
    }
    // STILLWATER_SIMULATE_LOW_MEMORY: pretend memory is low (for testing the unloader).
    status.dwMemoryLoad >= 92 || std::env::var_os("STILLWATER_SIMULATE_LOW_MEMORY").is_some()
}

#[cfg(not(windows))]
pub(crate) fn system_memory_low() -> bool {
    false
}
