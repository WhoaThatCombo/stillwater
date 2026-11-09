/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! A sampling allocation profiler for Windows (cargo feature `alloc-profile`).
//!
//! It is compiled in only with the feature and is active only when the environment variable
//! `STILLWATER_ALLOC_PROFILE=<output dir>` is set when the process starts. It then:
//!
//! - samples allocations by bytes: each thread counts down an exponentially distributed number
//!   of bytes (mean `STILLWATER_ALLOC_PROFILE_RATE`, default 256 KiB), and the allocation that
//!   crosses zero is sampled. An allocation of `size` bytes is thus sampled with probability
//!   `p = 1 - exp(-size / mean)` and is given the weight `size / p`, so the sum of the weights of
//!   the live samples is an unbiased estimate of the live bytes (big allocations are always sampled);
//! - covers Rust's global allocator and, by patching servoshell.exe's import table, the C runtime
//!   heap functions (`malloc`, `free`, ...) used by the C/C++ code linked into the executable
//!   (SpiderMonkey is built with `--disable-jemalloc`, so it uses these, as do ANGLE and HarfBuzz).
//!   Other DLLs (GStreamer, the GPU driver, system DLLs) are not covered;
//! - for a sampled allocation, records the raw stack (`RtlCaptureStackBackTrace`) in tables that
//!   live in `VirtualAlloc`ed memory and never go through the profiled path;
//! - writes a dump on request: a background thread watches the output directory for
//!   `dump-<name>.request` files and answers each with `dump-<name>.json`, holding the live samples
//!   aggregated by stack (raw addresses plus the module list, symbolized offline), how much of the
//!   sampled blocks is resident and how much of that is all zero, and the process memory counters
//!   (private bytes, private working set, process heap summaries) to compare coverage against.

use std::alloc::{GlobalAlloc, Layout};
use std::cell::{Cell, UnsafeCell};
use std::collections::HashMap;
use std::ffi::{OsString, c_void};
use std::io::Write;
use std::mem::transmute;
use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::ptr::null_mut;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::atomic::{AtomicU8, AtomicU32, AtomicUsize};
use std::sync::{Mutex, MutexGuard};

const TABLE_BITS: u32 = 17;
const TABLE_CAP: usize = 1 << TABLE_BITS;
const STACK_BITS: u32 = 18;
const STACK_CAP: usize = 1 << STACK_BITS;
const ARENA_FRAMES: usize = 4 << 20;
const FILTER_BITS: u32 = 18;
const MAX_FRAMES: usize = 62;
const PAGE: usize = 4096;

const UNINIT: u8 = 0;
const INITIALIZING: u8 = 1;
const OFF: u8 = 2;
const ON: u8 = 3;
static STATE: AtomicU8 = AtomicU8::new(UNINIT);
/// Mean sampling interval in bytes, as f64 bits.
static MEAN: AtomicU64 = AtomicU64::new(0);

const ORIGIN_RUST: u8 = 0;
const ORIGIN_CRT: u8 = 1;
const ORIGIN_HEAP: u8 = 2;
const ORIGIN_VIRTUAL: u8 = 3;
/// Raw commits (see `detour_ntdll`); in the dump they become RAW_UNATTRIBUTED or RAW_DUPLICATE.
const ORIGIN_RAW: u8 = 4;
const ORIGIN_RAW_DUPLICATE: u8 = 5;
const ORIGINS: [&str; 6] = ["rust", "crt", "heap", "virtual", "raw-unattributed", "raw-duplicate"];
static RAW_DETOURED: AtomicU8 = AtomicU8::new(0);

/// Counting filter: how many live samples hash to each bucket, so that a free can skip the lock.
static FILTER: [AtomicU32; 1 << FILTER_BITS] = [const { AtomicU32::new(0) }; 1 << FILTER_BITS];

thread_local! {
    static COUNTER: Cell<i64> = const { Cell::new(i64::MIN) };
    static RNG: Cell<u64> = const { Cell::new(0) };
    /// Set while the profiler itself runs on this thread (and always on the dump thread).
    static BUSY: Cell<bool> = const { Cell::new(false) };
}

#[derive(Clone, Copy)]
#[repr(C)]
struct Entry {
    ptr: usize,
    size: usize,
    weight: f64,
    stack: u32,
    origin: u8,
}

#[derive(Clone, Copy)]
#[repr(C)]
struct StackSlot {
    hash: u32,
    len: u32,
    offset: u32,
    used: u32,
}

struct Tables {
    entries: *mut Entry,
    stacks: *mut StackSlot,
    arena: *mut usize,
    arena_len: usize,
    live: usize,
    stack_count: usize,
    dropped: u64,
}
unsafe impl Send for Tables {}

static TABLES: Mutex<Tables> = Mutex::new(Tables {
    entries: null_mut(),
    stacks: null_mut(),
    arena: null_mut(),
    arena_len: 0,
    live: 0,
    stack_count: 0,
    dropped: 0,
});

struct OutDir(UnsafeCell<[u16; 1024]>, AtomicUsize);
unsafe impl Sync for OutDir {}
static OUT_DIR: OutDir = OutDir(UnsafeCell::new([0; 1024]), AtomicUsize::new(0));

#[repr(C)]
struct ProcessMemoryCounters {
    cb: u32,
    page_fault_count: u32,
    peak_working_set_size: usize,
    working_set_size: usize,
    quota_peak_paged_pool_usage: usize,
    quota_paged_pool_usage: usize,
    quota_peak_non_paged_pool_usage: usize,
    quota_non_paged_pool_usage: usize,
    pagefile_usage: usize,
    peak_pagefile_usage: usize,
    private_usage: usize,
    private_working_set_size: usize,
    shared_commit_usage: u64,
}

#[repr(C)]
struct HeapSummaryInfo {
    cb: u32,
    allocated: usize,
    committed: usize,
    reserved: usize,
    max_reserve: usize,
}

#[repr(C)]
struct ModuleInfo {
    base: *mut c_void,
    size: u32,
    entry: *mut c_void,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct WorkingSetExInfo {
    address: *mut c_void,
    attributes: usize,
}

#[repr(C)]
struct MemoryBasicInformation {
    base: usize,
    allocation_base: usize,
    allocation_protect: u32,
    partition_id: u16,
    region_size: usize,
    state: u32,
    protect: u32,
    kind: u32,
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetProcAddress(module: *mut c_void, name: *const u8) -> *mut c_void;
    fn GetModuleHandleExW(flags: u32, name: *const u16, module: *mut *mut c_void) -> i32;
    fn FreeLibrary(module: *mut c_void) -> i32;
    fn K32GetModuleBaseNameW(
        process: *mut c_void,
        module: *mut c_void,
        name: *mut u16,
        size: u32,
    ) -> u32;
    fn VirtualQuery(address: usize, info: *mut MemoryBasicInformation, size: usize) -> usize;
    fn ReadProcessMemory(
        process: *mut c_void,
        address: usize,
        buffer: *mut c_void,
        size: usize,
        read: *mut usize,
    ) -> i32;
    fn GetEnvironmentVariableW(name: *const u16, buffer: *mut u16, size: u32) -> u32;
    fn VirtualAlloc(address: *mut c_void, size: usize, kind: u32, protect: u32) -> *mut c_void;
    fn VirtualProtect(address: *mut c_void, size: usize, protect: u32, old: *mut u32) -> i32;
    fn GetModuleHandleW(name: *const u16) -> *mut c_void;
    fn CreateThread(
        attributes: *mut c_void,
        stack_size: usize,
        start: unsafe extern "system" fn(*mut c_void) -> u32,
        parameter: *mut c_void,
        flags: u32,
        thread_id: *mut u32,
    ) -> *mut c_void;
    fn CloseHandle(handle: *mut c_void) -> i32;
    fn RtlCaptureStackBackTrace(
        skip: u32,
        count: u32,
        frames: *mut *mut c_void,
        hash: *mut u32,
    ) -> u16;
    fn GetCurrentProcess() -> *mut c_void;
    fn GetCurrentProcessId() -> u32;
    fn GetProcessHeap() -> *mut c_void;
    fn GetProcessHeaps(count: u32, heaps: *mut *mut c_void) -> u32;
    fn HeapSummary(heap: *mut c_void, flags: u32, summary: *mut HeapSummaryInfo) -> i32;
    fn K32GetProcessMemoryInfo(
        process: *mut c_void,
        counters: *mut ProcessMemoryCounters,
        cb: u32,
    ) -> i32;
    fn K32QueryWorkingSetEx(process: *mut c_void, info: *mut c_void, cb: u32) -> i32;
    fn K32EnumProcessModules(
        process: *mut c_void,
        modules: *mut *mut c_void,
        cb: u32,
        needed: *mut u32,
    ) -> i32;
    fn K32GetModuleInformation(
        process: *mut c_void,
        module: *mut c_void,
        info: *mut ModuleInfo,
        cb: u32,
    ) -> i32;
    fn K32GetModuleFileNameExW(
        process: *mut c_void,
        module: *mut c_void,
        name: *mut u16,
        size: u32,
    ) -> u32;
}

/// Rust's global allocator, wrapped.
pub struct Profiled<A>(pub A);

unsafe impl<A: GlobalAlloc> GlobalAlloc for Profiled<A> {
    #[inline]
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { self.0.alloc(layout) };
        on_alloc(ptr, layout.size(), ORIGIN_RUST);
        ptr
    }

    #[inline]
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { self.0.alloc_zeroed(layout) };
        on_alloc(ptr, layout.size(), ORIGIN_RUST);
        ptr
    }

    #[inline]
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        on_free(ptr);
        unsafe { self.0.dealloc(ptr, layout) }
    }

    #[inline]
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // Forget the old block first, so that every block in the table is live while the dump
        // thread holds the lock.
        on_free(ptr);
        let new = unsafe { self.0.realloc(ptr, layout, new_size) };
        on_alloc(new, new_size, ORIGIN_RUST);
        new
    }
}

#[inline]
fn on_alloc(ptr: *mut u8, size: usize, origin: u8) {
    let state = STATE.load(Relaxed);
    if state != ON {
        if state == UNINIT {
            init();
        }
        if STATE.load(Relaxed) != ON {
            return;
        }
    }
    if ptr.is_null() {
        return;
    }
    let sample = COUNTER
        .try_with(|counter| {
            let mut left = counter.get();
            if left == i64::MIN {
                left = next_interval();
            }
            left -= size as i64;
            if left > 0 {
                counter.set(left);
                false
            } else {
                counter.set(next_interval());
                true
            }
        })
        .unwrap_or(false);
    if sample {
        record(ptr, size, origin);
    }
}

#[inline]
fn on_free(ptr: *mut u8) {
    if STATE.load(Relaxed) != ON || ptr.is_null() {
        return;
    }
    if FILTER[filter_index(ptr as usize)].load(Relaxed) == 0 {
        return;
    }
    lock().remove(ptr as usize);
}

fn mix(ptr: usize) -> u64 {
    ((ptr as u64) >> 4).wrapping_mul(0x9E37_79B9_7F4A_7C15)
}

fn filter_index(ptr: usize) -> usize {
    (mix(ptr) >> (64 - FILTER_BITS)) as usize
}

fn mean() -> f64 {
    f64::from_bits(MEAN.load(Relaxed))
}

fn next_interval() -> i64 {
    let mut x = RNG.try_with(|r| r.get()).unwrap_or(0);
    if x == 0 {
        x = unsafe { core::arch::x86_64::_rdtsc() } ^ (&x as *const u64 as u64) | 1;
    }
    x ^= x >> 12;
    x ^= x << 25;
    x ^= x >> 27;
    let _ = RNG.try_with(|r| r.set(x));
    let bits = x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11;
    let uniform = (bits as f64 + 1.0) / (1u64 << 53) as f64; // (0, 1]
    (-uniform.ln() * mean()) as i64 + 1
}

fn lock() -> MutexGuard<'static, Tables> {
    TABLES.lock().unwrap_or_else(|e| e.into_inner())
}

#[inline(never)]
fn record(ptr: *mut u8, size: usize, origin: u8) {
    if BUSY.try_with(|busy| busy.replace(true)).unwrap_or(true) {
        return;
    }
    let mean = mean();
    let probability = 1.0 - (-(size as f64) / mean).exp();
    let weight = if probability > 0.0 {
        size as f64 / probability
    } else {
        mean
    };
    let mut frames = [null_mut::<c_void>(); MAX_FRAMES];
    let mut hash = 0u32;
    let count = unsafe {
        RtlCaptureStackBackTrace(1, MAX_FRAMES as u32, frames.as_mut_ptr(), &mut hash)
    } as usize;
    {
        let mut tables = lock();
        let stack = tables.intern(&frames[..count], hash);
        tables.insert(ptr as usize, size, weight, stack, origin);
    }
    let _ = BUSY.try_with(|busy| busy.set(false));
}

impl Tables {
    /// Only called with the lock held.
    fn entries(&self) -> &'static mut [Entry] {
        unsafe { std::slice::from_raw_parts_mut(self.entries, TABLE_CAP) }
    }

    fn home(ptr: usize) -> usize {
        (mix(ptr) >> (64 - TABLE_BITS)) as usize
    }

    fn intern(&mut self, frames: &[*mut c_void], hash: u32) -> u32 {
        let mask = STACK_CAP - 1;
        let mut index = (hash as usize).wrapping_mul(0x9E37_79B9) & mask;
        loop {
            let slot = unsafe { *self.stacks.add(index) };
            if slot.used == 0 {
                break;
            }
            if slot.hash == hash && slot.len as usize == frames.len() {
                let stored = unsafe {
                    std::slice::from_raw_parts(self.arena.add(slot.offset as usize), frames.len())
                };
                if stored.iter().zip(frames).all(|(a, b)| *a == *b as usize) {
                    return index as u32;
                }
            }
            index = (index + 1) & mask;
        }
        if self.stack_count >= STACK_CAP * 3 / 4 || self.arena_len + frames.len() > ARENA_FRAMES
        {
            return 0;
        }
        for (i, frame) in frames.iter().enumerate() {
            unsafe { *self.arena.add(self.arena_len + i) = *frame as usize };
        }
        unsafe {
            *self.stacks.add(index) = StackSlot {
                hash,
                len: frames.len() as u32,
                offset: self.arena_len as u32,
                used: 1,
            };
        }
        self.arena_len += frames.len();
        self.stack_count += 1;
        index as u32
    }

    fn insert(&mut self, ptr: usize, size: usize, weight: f64, stack: u32, origin: u8) {
        let mask = TABLE_CAP - 1;
        let mut index = Self::home(ptr);
        let full = self.live >= TABLE_CAP * 3 / 4;
        let entries = self.entries();
        loop {
            let entry = &mut entries[index];
            if entry.ptr == ptr {
                // A stale entry: the block was freed by a path we do not see.
                *entry = Entry { ptr, size, weight, stack, origin };
                return;
            }
            if entry.ptr == 0 {
                if full {
                    self.dropped += 1;
                    return;
                }
                *entry = Entry { ptr, size, weight, stack, origin };
                break;
            }
            index = (index + 1) & mask;
        }
        self.live += 1;
        FILTER[filter_index(ptr)].fetch_add(1, Relaxed);
    }

    fn remove(&mut self, ptr: usize) {
        let mask = TABLE_CAP - 1;
        let entries = self.entries();
        let mut i = Self::home(ptr);
        loop {
            if entries[i].ptr == 0 {
                return;
            }
            if entries[i].ptr == ptr {
                break;
            }
            i = (i + 1) & mask;
        }
        // Backward-shift deletion (linear probing without tombstones).
        loop {
            let mut j = i;
            loop {
                j = (j + 1) & mask;
                if entries[j].ptr == 0 {
                    entries[i].ptr = 0;
                    self.live -= 1;
                    FILTER[filter_index(ptr)].fetch_sub(1, Relaxed);
                    return;
                }
                let k = Self::home(entries[j].ptr);
                let between = if i <= j { i < k && k <= j } else { i < k || k <= j };
                if !between {
                    break;
                }
            }
            entries[i] = entries[j];
            i = j;
        }
    }
}

fn env(name: &str, buffer: &mut [u16]) -> Option<usize> {
    let mut wide = [0u16; 64];
    for (i, c) in name.encode_utf16().enumerate() {
        wide[i] = c;
    }
    let len = unsafe { GetEnvironmentVariableW(wide.as_ptr(), buffer.as_mut_ptr(), buffer.len() as u32) };
    (len > 0 && (len as usize) < buffer.len()).then_some(len as usize)
}

#[cold]
#[inline(never)]
fn init() {
    if STATE.compare_exchange(UNINIT, INITIALIZING, Relaxed, Relaxed).is_err() {
        return;
    }
    let dir = unsafe { &mut *OUT_DIR.0.get() };
    let Some(len) = env("STILLWATER_ALLOC_PROFILE", dir) else {
        STATE.store(OFF, Relaxed);
        return;
    };
    OUT_DIR.1.store(len, Relaxed);
    let mut rate = [0u16; 32];
    let mean = env("STILLWATER_ALLOC_PROFILE_RATE", &mut rate)
        .and_then(|n| String::from_utf16_lossy(&rate[..n]).trim().parse::<f64>().ok())
        .filter(|value| *value >= 1.0)
        .unwrap_or(256.0 * 1024.0);
    MEAN.store(mean.to_bits(), Relaxed);
    const RESERVE_COMMIT: u32 = 0x1000 | 0x2000;
    const READWRITE: u32 = 0x04;
    unsafe {
        let mut tables = lock();
        tables.entries = VirtualAlloc(
            null_mut(),
            TABLE_CAP * size_of::<Entry>(),
            RESERVE_COMMIT,
            READWRITE,
        )
        .cast();
        tables.stacks = VirtualAlloc(
            null_mut(),
            STACK_CAP * size_of::<StackSlot>(),
            RESERVE_COMMIT,
            READWRITE,
        )
        .cast();
        tables.arena =
            VirtualAlloc(null_mut(), ARENA_FRAMES * 8, RESERVE_COMMIT, READWRITE).cast();
        if tables.entries.is_null() || tables.stacks.is_null() || tables.arena.is_null() {
            STATE.store(OFF, Relaxed);
            return;
        }
        // Stack 0 means "unknown" (the stack table is full).
        *tables.stacks = StackSlot { hash: 0, len: 0, offset: 0, used: 1 };
        tables.stack_count = 1;
    }
    // The executable's imports right away; DLLs from the watcher thread (also those loaded later).
    unsafe { patch_module(GetModuleHandleW(std::ptr::null()).cast(), true) };
    let mut raw = [0u16; 8];
    if env("STILLWATER_ALLOC_PROFILE_RAW", &mut raw) != Some(1) || raw[0] != u16::from(b'0') {
        RAW_DETOURED.store(unsafe { detour_ntdll() } as u8, Relaxed);
    }
    STATE.store(ON, Relaxed);
    let thread = unsafe { CreateThread(null_mut(), 0, watcher, null_mut(), 0, null_mut()) };
    if !thread.is_null() {
        unsafe { CloseHandle(thread) };
    }
}

// Imported allocation functions that get hooked, by family. The executable gets the CRT and
// VirtualAlloc families (its HeapAlloc calls are Rust's System allocator, already counted); every
// other module (DLLs, including ones loaded later, such as GStreamer plugins and GPU drivers) gets
// all families. The CRT DLLs and the core system DLLs are left alone: their own heap calls are the
// implementation of what is hooked in their callers.
const UCRT: usize = 0; // api-ms-win-crt-heap-*.dll, ucrtbase.dll
const MSVCRT: usize = 1; // msvcrt.dll (MinGW-built DLLs)
const HEAP: usize = 2; // HeapAlloc, HeapReAlloc, HeapFree
const RTL_HEAP: usize = 3; // RtlAllocateHeap, RtlReAllocateHeap, RtlFreeHeap (ntdll)
const VIRTUAL: usize = 4; // VirtualAlloc, VirtualFree
const NT_VIRTUAL: usize = 5; // NtAllocateVirtualMemory, NtFreeVirtualMemory (ntdll)
const FAMILIES: usize = 6;

const CRT_NAMES: [&[u8]; 8] = [
    b"malloc",
    b"calloc",
    b"realloc",
    b"free",
    b"_aligned_malloc",
    b"_aligned_realloc",
    b"_aligned_free",
    b"_recalloc",
];
const FUNCTIONS: [&[&[u8]]; FAMILIES] = [
    &CRT_NAMES,
    &CRT_NAMES,
    &[b"HeapAlloc", b"HeapReAlloc", b"HeapFree"],
    &[b"RtlAllocateHeap", b"RtlReAllocateHeap", b"RtlFreeHeap"],
    &[b"VirtualAlloc", b"VirtualFree"],
    &[b"NtAllocateVirtualMemory", b"NtFreeVirtualMemory"],
];
/// Where the real functions are looked up (forwarders are resolved by GetProcAddress).
const FAMILY_DLLS: [&str; FAMILIES] =
    ["ucrtbase.dll", "msvcrt.dll", "kernelbase.dll", "ntdll.dll", "kernelbase.dll", "ntdll.dll"];
static REAL: [[AtomicUsize; 8]; FAMILIES] =
    [const { [const { AtomicUsize::new(0) }; 8] }; FAMILIES];
static PATCHED_IMPORTS: AtomicUsize = AtomicUsize::new(0);
static HOOKED_MODULES: AtomicUsize = AtomicUsize::new(0);

const MEM_COMMIT: u32 = 0x1000;

fn real(family: usize, index: usize) -> usize {
    REAL[family][index].load(Relaxed)
}

unsafe extern "C" fn crt_malloc<const F: usize>(size: usize) -> *mut c_void {
    let f: unsafe extern "C" fn(usize) -> *mut c_void = unsafe { transmute(real(F, 0)) };
    let ptr = unsafe { f(size) };
    on_alloc(ptr.cast(), size, ORIGIN_CRT);
    ptr
}

unsafe extern "C" fn crt_calloc<const F: usize>(count: usize, size: usize) -> *mut c_void {
    let f: unsafe extern "C" fn(usize, usize) -> *mut c_void = unsafe { transmute(real(F, 1)) };
    let ptr = unsafe { f(count, size) };
    on_alloc(ptr.cast(), count.saturating_mul(size), ORIGIN_CRT);
    ptr
}

unsafe extern "C" fn crt_realloc<const F: usize>(old: *mut c_void, size: usize) -> *mut c_void {
    let f: unsafe extern "C" fn(*mut c_void, usize) -> *mut c_void =
        unsafe { transmute(real(F, 2)) };
    on_free(old.cast());
    let ptr = unsafe { f(old, size) };
    on_alloc(ptr.cast(), size, ORIGIN_CRT);
    ptr
}

unsafe extern "C" fn crt_free<const F: usize>(ptr: *mut c_void) {
    let f: unsafe extern "C" fn(*mut c_void) = unsafe { transmute(real(F, 3)) };
    on_free(ptr.cast());
    unsafe { f(ptr) }
}

unsafe extern "C" fn crt_aligned_malloc<const F: usize>(size: usize, align: usize) -> *mut c_void {
    let f: unsafe extern "C" fn(usize, usize) -> *mut c_void = unsafe { transmute(real(F, 4)) };
    let ptr = unsafe { f(size, align) };
    on_alloc(ptr.cast(), size, ORIGIN_CRT);
    ptr
}

unsafe extern "C" fn crt_aligned_realloc<const F: usize>(
    old: *mut c_void,
    size: usize,
    align: usize,
) -> *mut c_void {
    let f: unsafe extern "C" fn(*mut c_void, usize, usize) -> *mut c_void =
        unsafe { transmute(real(F, 5)) };
    on_free(old.cast());
    let ptr = unsafe { f(old, size, align) };
    on_alloc(ptr.cast(), size, ORIGIN_CRT);
    ptr
}

unsafe extern "C" fn crt_aligned_free<const F: usize>(ptr: *mut c_void) {
    let f: unsafe extern "C" fn(*mut c_void) = unsafe { transmute(real(F, 6)) };
    on_free(ptr.cast());
    unsafe { f(ptr) }
}

unsafe extern "C" fn crt_recalloc<const F: usize>(
    old: *mut c_void,
    count: usize,
    size: usize,
) -> *mut c_void {
    let f: unsafe extern "C" fn(*mut c_void, usize, usize) -> *mut c_void =
        unsafe { transmute(real(F, 7)) };
    on_free(old.cast());
    let ptr = unsafe { f(old, count, size) };
    on_alloc(ptr.cast(), count.saturating_mul(size), ORIGIN_CRT);
    ptr
}

unsafe extern "system" fn heap_alloc<const F: usize>(
    heap: *mut c_void,
    flags: u32,
    size: usize,
) -> *mut c_void {
    let f: unsafe extern "system" fn(*mut c_void, u32, usize) -> *mut c_void =
        unsafe { transmute(real(F, 0)) };
    let ptr = unsafe { f(heap, flags, size) };
    on_alloc(ptr.cast(), size, ORIGIN_HEAP);
    ptr
}

unsafe extern "system" fn heap_realloc<const F: usize>(
    heap: *mut c_void,
    flags: u32,
    old: *mut c_void,
    size: usize,
) -> *mut c_void {
    let f: unsafe extern "system" fn(*mut c_void, u32, *mut c_void, usize) -> *mut c_void =
        unsafe { transmute(real(F, 1)) };
    on_free(old.cast());
    let ptr = unsafe { f(heap, flags, old, size) };
    on_alloc(ptr.cast(), size, ORIGIN_HEAP);
    ptr
}

/// Returns the real function's whole return register (BOOL for HeapFree, BOOLEAN for RtlFreeHeap).
unsafe extern "system" fn heap_free<const F: usize>(
    heap: *mut c_void,
    flags: u32,
    ptr: *mut c_void,
) -> usize {
    let f: unsafe extern "system" fn(*mut c_void, u32, *mut c_void) -> usize =
        unsafe { transmute(real(F, 2)) };
    on_free(ptr.cast());
    unsafe { f(heap, flags, ptr) }
}

unsafe extern "system" fn virtual_alloc(
    address: *mut c_void,
    size: usize,
    kind: u32,
    protect: u32,
) -> *mut c_void {
    let f: unsafe extern "system" fn(*mut c_void, usize, u32, u32) -> *mut c_void =
        unsafe { transmute(real(VIRTUAL, 0)) };
    let ptr = unsafe { f(address, size, kind, protect) };
    if kind & MEM_COMMIT != 0 {
        on_alloc(ptr.cast(), size, ORIGIN_VIRTUAL);
    }
    ptr
}

/// Forgets a commit when its base address is released or decommitted (a partial decommit at
/// another address is not seen, but the dump only counts pages that are still resident).
unsafe extern "system" fn virtual_free(address: *mut c_void, size: usize, kind: u32) -> i32 {
    let f: unsafe extern "system" fn(*mut c_void, usize, u32) -> i32 =
        unsafe { transmute(real(VIRTUAL, 1)) };
    on_free(address.cast());
    unsafe { f(address, size, kind) }
}

unsafe extern "system" fn nt_allocate(
    process: *mut c_void,
    base: *mut *mut c_void,
    zero_bits: usize,
    size: *mut usize,
    kind: u32,
    protect: u32,
) -> i32 {
    let f: unsafe extern "system" fn(*mut c_void, *mut *mut c_void, usize, *mut usize, u32, u32) -> i32 =
        unsafe { transmute(real(NT_VIRTUAL, 0)) };
    let status = unsafe { f(process, base, zero_bits, size, kind, protect) };
    if status >= 0 && process as isize == -1 && kind & MEM_COMMIT != 0 && !base.is_null() && !size.is_null() {
        on_alloc(unsafe { *base }.cast(), unsafe { *size }, ORIGIN_VIRTUAL);
    }
    status
}

unsafe extern "system" fn nt_free(
    process: *mut c_void,
    base: *mut *mut c_void,
    size: *mut usize,
    kind: u32,
) -> i32 {
    let f: unsafe extern "system" fn(*mut c_void, *mut *mut c_void, *mut usize, u32) -> i32 =
        unsafe { transmute(real(NT_VIRTUAL, 1)) };
    if process as isize == -1 && !base.is_null() {
        on_free(unsafe { *base }.cast());
    }
    unsafe { f(process, base, size, kind) }
}

fn hooks(family: usize) -> [usize; 8] {
    macro_rules! crt {
        ($f:expr) => {
            [
                crt_malloc::<$f> as *const () as usize,
                crt_calloc::<$f> as *const () as usize,
                crt_realloc::<$f> as *const () as usize,
                crt_free::<$f> as *const () as usize,
                crt_aligned_malloc::<$f> as *const () as usize,
                crt_aligned_realloc::<$f> as *const () as usize,
                crt_aligned_free::<$f> as *const () as usize,
                crt_recalloc::<$f> as *const () as usize,
            ]
        };
    }
    macro_rules! heap {
        ($f:expr) => {
            [
                heap_alloc::<$f> as *const () as usize,
                heap_realloc::<$f> as *const () as usize,
                heap_free::<$f> as *const () as usize,
                0,
                0,
                0,
                0,
                0,
            ]
        };
    }
    match family {
        UCRT => crt!(UCRT),
        MSVCRT => crt!(MSVCRT),
        HEAP => heap!(HEAP),
        RTL_HEAP => heap!(RTL_HEAP),
        NT_VIRTUAL => [
            nt_allocate as *const () as usize,
            nt_free as *const () as usize,
            0,
            0,
            0,
            0,
            0,
            0,
        ],
        _ => [
            virtual_alloc as *const () as usize,
            virtual_free as *const () as usize,
            0,
            0,
            0,
            0,
            0,
            0,
        ],
    }
}

/// The real function, looked up once in the DLL that implements it.
unsafe fn resolve(family: usize, index: usize) -> usize {
    let known = real(family, index);
    if known != 0 {
        return known;
    }
    let mut dll = [0u16; 32];
    for (i, c) in FAMILY_DLLS[family].encode_utf16().enumerate() {
        dll[i] = c;
    }
    let mut name = [0u8; 32];
    name[..FUNCTIONS[family][index].len()].copy_from_slice(FUNCTIONS[family][index]);
    let module = unsafe { GetModuleHandleW(dll.as_ptr()) };
    if module.is_null() {
        return 0;
    }
    let address = unsafe { GetProcAddress(module, name.as_ptr()) } as usize;
    if address != 0 {
        REAL[family][index].store(address, Relaxed);
    }
    address
}

/// The families to look for among a module's imports of `dll` (lowercase).
fn import_families(dll: &[u8]) -> &'static [usize] {
    if dll.starts_with(b"api-ms-win-crt-heap-") || dll == b"ucrtbase.dll" {
        &[UCRT]
    } else if dll == b"msvcrt.dll" {
        &[MSVCRT]
    } else if dll == b"kernel32.dll" || dll == b"kernelbase.dll" || dll.starts_with(b"api-ms-win-core-")
    {
        &[HEAP, VIRTUAL]
    } else if dll == b"ntdll.dll" {
        &[RTL_HEAP, NT_VIRTUAL]
    } else {
        &[]
    }
}

/// Points a module's imports of the allocation functions at the hooks. Returns how many import
/// slots were patched.
unsafe fn patch_module(base: *const u8, is_exe: bool) -> usize {
    let allowed = |family: usize| !is_exe || matches!(family, UCRT | MSVCRT | VIRTUAL | NT_VIRTUAL);
    let mut patched = 0;
    unsafe {
        let read_u32 = |offset: usize| (base.add(offset) as *const u32).read_unaligned() as usize;
        if (base as *const u16).read_unaligned() != 0x5A4D {
            return 0;
        }
        let nt = read_u32(0x3c);
        // IMAGE_NT_HEADERS64: signature (4) + file header (20), then the optional header (PE32+
        // magic 0x20b) whose data directories start at offset 112; imports are entry 1.
        if (base.add(nt + 24) as *const u16).read_unaligned() != 0x20b {
            return 0;
        }
        let imports = read_u32(nt + 24 + 112 + 8);
        if imports == 0 {
            return 0;
        }
        let mut descriptor = imports;
        loop {
            let names = read_u32(descriptor);
            let dll_name = read_u32(descriptor + 12);
            let thunks = read_u32(descriptor + 16);
            if dll_name == 0 {
                break;
            }
            descriptor += 20;
            // Without the name table (bound imports only) the functions cannot be told apart.
            if names == 0 || thunks == 0 {
                continue;
            }
            let mut dll = [0u8; 64];
            let raw = std::ffi::CStr::from_ptr(base.add(dll_name).cast()).to_bytes();
            let len = raw.len().min(dll.len());
            dll[..len].copy_from_slice(&raw[..len]);
            dll[..len].make_ascii_lowercase();
            let families = import_families(&dll[..len]);
            if !families.iter().any(|f| allowed(*f)) {
                continue;
            }
            let mut i = 0;
            loop {
                let entry = (base.add(names + i * 8) as *const u64).read_unaligned();
                if entry == 0 {
                    break;
                }
                let slot = base.add(thunks + i * 8) as *mut usize;
                i += 1;
                if entry & (1 << 63) != 0 {
                    continue;
                }
                let function = std::ffi::CStr::from_ptr(base.add(entry as usize + 2).cast()).to_bytes();
                for &family in families.iter().filter(|f| allowed(**f)) {
                    let Some(index) = FUNCTIONS[family].iter().position(|f| *f == function) else {
                        continue;
                    };
                    let hook = hooks(family)[index];
                    if slot.read_volatile() == hook || resolve(family, index) == 0 {
                        continue;
                    }
                    let mut old = 0;
                    if VirtualProtect(slot.cast(), 8, 0x04, &mut old) != 0 {
                        slot.write_volatile(hook);
                        VirtualProtect(slot.cast(), 8, old, &mut old);
                        patched += 1;
                    }
                }
            }
        }
    }
    PATCHED_IMPORTS.fetch_add(patched, Relaxed);
    patched
}

/// Hooks the modules loaded since the last call (on the watcher thread).
fn patch_new_modules(done: &mut Vec<usize>) {
    let process = unsafe { GetCurrentProcess() };
    let mut modules = [null_mut::<c_void>(); 1024];
    let mut needed = 0u32;
    unsafe {
        K32EnumProcessModules(process, modules.as_mut_ptr(), (modules.len() * 8) as u32, &mut needed)
    };
    let exe = unsafe { GetModuleHandleW(std::ptr::null()) };
    for module in &modules[..(needed as usize / 8).min(modules.len())] {
        if done.contains(&(*module as usize)) {
            continue;
        }
        done.push(*module as usize);
        let mut name = [0u16; 260];
        let len = unsafe { K32GetModuleBaseNameW(process, *module, name.as_mut_ptr(), 260) } as usize;
        let name = String::from_utf16_lossy(&name[..len]).to_ascii_lowercase();
        if *module == exe ||
            name.starts_with("api-ms-") ||
            matches!(
                name.as_str(),
                "ntdll.dll" | "kernel32.dll" | "kernelbase.dll" | "ucrtbase.dll" | "msvcrt.dll"
            )
        {
            continue;
        }
        // Pin the module while its import table is patched.
        let mut pinned = null_mut();
        if unsafe { GetModuleHandleExW(0x4, (*module).cast(), &mut pinned) } == 0 {
            continue;
        }
        if unsafe { patch_module((*module).cast(), false) } > 0 {
            HOOKED_MODULES.fetch_add(1, Relaxed);
        }
        unsafe { FreeLibrary(pinned) };
    }
}

// Diagnostic: every commit of virtual memory in the process, whoever makes it (the heap's own
// segments, kernelbase's VirtualAlloc, code that resolves functions with GetProcAddress, ...), by
// detouring ntdll's NtAllocateVirtualMemory / NtFreeVirtualMemory. These samples (origin "raw")
// overlap the others, so they are not added to the totals: the dump only uses them to name the
// owners of reservations that nothing else accounts for.
static RAW_SYSCALLS: [AtomicUsize; 3] = [const { AtomicUsize::new(0) }; 3];

fn tls_ready() -> bool {
    // The loader allocates a new thread's TLS block with the heap, which may get here before the
    // block exists: touch no thread-local until it does.
    let tls: usize;
    unsafe { std::arch::asm!("mov {}, gs:[0x58]", out(reg) tls, options(nostack, readonly, preserves_flags)) };
    tls != 0
}

unsafe extern "system" fn raw_allocate_ex(
    process: *mut c_void,
    base: *mut *mut c_void,
    size: *mut usize,
    kind: u32,
    protect: u32,
    parameters: *mut c_void,
    count: u32,
) -> i32 {
    let f: unsafe extern "system" fn(*mut c_void, *mut *mut c_void, *mut usize, u32, u32, *mut c_void, u32) -> i32 =
        unsafe { transmute(RAW_SYSCALLS[2].load(Relaxed)) };
    let status = unsafe { f(process, base, size, kind, protect, parameters, count) };
    if tls_ready() && status >= 0 && process as isize == -1 && kind & MEM_COMMIT != 0 && !base.is_null() && !size.is_null() {
        on_alloc(unsafe { *base }.cast(), unsafe { *size }, ORIGIN_RAW);
    }
    status
}

unsafe extern "system" fn raw_allocate(
    process: *mut c_void,
    base: *mut *mut c_void,
    zero_bits: usize,
    size: *mut usize,
    kind: u32,
    protect: u32,
) -> i32 {
    let f: unsafe extern "system" fn(*mut c_void, *mut *mut c_void, usize, *mut usize, u32, u32) -> i32 =
        unsafe { transmute(RAW_SYSCALLS[0].load(Relaxed)) };
    let status = unsafe { f(process, base, zero_bits, size, kind, protect) };
    if tls_ready() && status >= 0 && process as isize == -1 && kind & MEM_COMMIT != 0 && !base.is_null() && !size.is_null() {
        on_alloc(unsafe { *base }.cast(), unsafe { *size }, ORIGIN_RAW);
    }
    status
}

unsafe extern "system" fn raw_free(
    process: *mut c_void,
    base: *mut *mut c_void,
    size: *mut usize,
    kind: u32,
) -> i32 {
    let f: unsafe extern "system" fn(*mut c_void, *mut *mut c_void, *mut usize, u32) -> i32 =
        unsafe { transmute(RAW_SYSCALLS[1].load(Relaxed)) };
    if process as isize == -1 && !base.is_null() {
        on_free(unsafe { *base }.cast());
    }
    unsafe { f(process, base, size, kind) }
}

/// Detours the two ntdll syscall stubs. Each stub starts with `mov r10, rcx; mov eax, <number>`;
/// its first 8 bytes are replaced in one aligned store by a 5-byte jump to a page near ntdll,
/// which jumps on to the Rust function; that calls a copy of the syscall (`mov r10, rcx; mov eax,
/// <number>; syscall; ret`) on the same page.
unsafe fn detour_ntdll() -> bool {
    let mut name = [0u16; 16];
    for (i, c) in "ntdll.dll".encode_utf16().enumerate() {
        name[i] = c;
    }
    unsafe {
        let ntdll = GetModuleHandleW(name.as_ptr());
        let stubs = [
            GetProcAddress(ntdll, c"NtAllocateVirtualMemory".as_ptr().cast()) as *mut u8,
            GetProcAddress(ntdll, c"NtFreeVirtualMemory".as_ptr().cast()) as *mut u8,
            GetProcAddress(ntdll, c"NtAllocateVirtualMemoryEx".as_ptr().cast()) as *mut u8,
        ];
        for stub in stubs {
            if stub.is_null() ||
                !(stub as usize).is_multiple_of(8) ||
                std::slice::from_raw_parts(stub, 4) != [0x4C, 0x8B, 0xD1, 0xB8]
            {
                return false;
            }
        }
        // A page within a rel32 jump of ntdll.
        let mut page = null_mut::<u8>();
        let mut hint = (ntdll as usize & !0xFFFF).wrapping_sub(0x10000);
        for _ in 0..4096 {
            page = VirtualAlloc(hint as *mut c_void, PAGE, 0x3000, 0x40).cast();
            if !page.is_null() {
                break;
            }
            hint -= 0x10000;
        }
        if page.is_null() {
            return false;
        }
        let targets = [
            raw_allocate as *const () as usize,
            raw_free as *const () as usize,
            raw_allocate_ex as *const () as usize,
        ];
        for (i, stub) in stubs.into_iter().enumerate() {
            let number = (stub.add(4) as *const u32).read_unaligned();
            // At page + 64 * i: the syscall copy (16 bytes), then the absolute jump to Rust.
            let syscall = page.add(64 * i);
            let code = [0x4C, 0x8B, 0xD1, 0xB8];
            std::ptr::copy_nonoverlapping(code.as_ptr(), syscall, 4);
            (syscall.add(4) as *mut u32).write_unaligned(number);
            std::ptr::copy_nonoverlapping([0x0F, 0x05, 0xC3].as_ptr(), syscall.add(8), 3);
            RAW_SYSCALLS[i].store(syscall as usize, Relaxed);
            let jump = page.add(64 * i + 16);
            std::ptr::copy_nonoverlapping([0xFF, 0x25, 0, 0, 0, 0].as_ptr(), jump, 6);
            (jump.add(6) as *mut usize).write_unaligned(targets[i]);
            let relative = (jump as isize - (stub as isize + 5)) as i32;
            let mut head = (stub as *const u64).read();
            let mut bytes = head.to_le_bytes();
            bytes[0] = 0xE9;
            bytes[1..5].copy_from_slice(&relative.to_le_bytes());
            head = u64::from_le_bytes(bytes);
            let mut old = 0;
            if VirtualProtect(stub.cast(), 8, 0x40, &mut old) == 0 {
                return false;
            }
            (*(stub as *const std::sync::atomic::AtomicU64)).store(head, std::sync::atomic::Ordering::SeqCst);
            VirtualProtect(stub.cast(), 8, old, &mut old);
        }
    }
    true
}

fn out_dir() -> PathBuf {
    let len = OUT_DIR.1.load(Relaxed);
    let wide: &[u16; 1024] = unsafe { &*OUT_DIR.0.get() };
    PathBuf::from(OsString::from_wide(&wide[..len]))
}

unsafe extern "system" fn watcher(_: *mut c_void) -> u32 {
    let _ = BUSY.try_with(|busy| busy.set(true));
    let dir = out_dir();
    let _ = std::fs::create_dir_all(&dir);
    let pid = unsafe { GetCurrentProcessId() };
    let mut modules_done = Vec::new();
    patch_new_modules(&mut modules_done);
    let _ = std::fs::write(
        dir.join(format!("profiler-{pid}.txt")),
        format!(
            "pid {pid}\nmean sampling interval {} bytes\nimport slots hooked {} (DLLs hooked {})\n",
            mean(),
            PATCHED_IMPORTS.load(Relaxed),
            HOOKED_MODULES.load(Relaxed)
        ),
    );
    let mut snapshot: Vec<Sample> = Vec::with_capacity(TABLE_CAP);
    let mut tick = 0u64;
    loop {
        tick += 1;
        std::thread::sleep(std::time::Duration::from_millis(200));
        if tick.is_multiple_of(5) {
            patch_new_modules(&mut modules_done);
        }
        let Ok(listing) = std::fs::read_dir(&dir) else {
            continue;
        };
        let requests: Vec<String> = listing
            .flatten()
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| n.starts_with("dump-") && n.ends_with(".request"))
            .collect();
        for request in requests {
            // Only one process gets to remove the request.
            if std::fs::remove_file(dir.join(&request)).is_err() {
                continue;
            }
            let label = &request["dump-".len()..request.len() - ".request".len()];
            if let Err(error) = dump(&dir, label, &mut snapshot) {
                let _ = std::fs::write(dir.join(format!("dump-{label}.error")), error.to_string());
            }
        }
    }
}

#[derive(Clone, Copy)]
struct Sample {
    entry: Entry,
    /// Bytes in whole pages inside the block, of them resident, of them all zero.
    paged: usize,
    resident: usize,
    zero: usize,
}

/// Which whole pages of the block are resident, and which of those hold only zeros.
fn scan(ptr: usize, size: usize) -> (usize, usize, usize) {
    let first = (ptr + PAGE - 1) & !(PAGE - 1);
    let end = (ptr + size) & !(PAGE - 1);
    if end <= first {
        return (0, 0, 0);
    }
    let (mut resident, mut zero) = (0, 0);
    let mut info = [WorkingSetExInfo { address: null_mut(), attributes: 0 }; 512];
    let mut page = first;
    while page < end {
        let n = ((end - page) / PAGE).min(info.len());
        for (i, item) in info[..n].iter_mut().enumerate() {
            *item = WorkingSetExInfo { address: (page + i * PAGE) as *mut c_void, attributes: 0 };
        }
        let ok = unsafe {
            K32QueryWorkingSetEx(
                GetCurrentProcess(),
                info.as_mut_ptr().cast(),
                (n * size_of::<WorkingSetExInfo>()) as u32,
            )
        };
        if ok != 0 {
            for item in &info[..n] {
                if item.attributes & 1 != 0 {
                    resident += PAGE;
                    if page_is_zero(item.address as usize) {
                        zero += PAGE;
                    }
                }
            }
        }
        page += n * PAGE;
    }
    (end - first, resident, zero)
}

/// Reads through ReadProcessMemory, which fails instead of faulting if another thread just
/// released the page.
fn page_is_zero(address: usize) -> bool {
    let mut buffer = [0u64; PAGE / 8];
    let mut read = 0;
    let ok = unsafe {
        ReadProcessMemory(GetCurrentProcess(), address, buffer.as_mut_ptr().cast(), PAGE, &mut read)
    };
    ok != 0 && read == PAGE && buffer.iter().all(|w| *w == 0)
}

const CENSUS_CLASSES: [&str; 9] = [
    "process heap (segments and big blocks)",
    "thread stacks (reservations with a guard page)",
    "sampled VirtualAlloc commits (attributed above)",
    "GC chunks not sampled (1 MB aligned, 1 MB)",
    "profiler tables",
    "other reservations up to 1 MB",
    "other reservations 1-16 MB",
    "other reservations over 16 MB",
    "other, executable",
];

#[repr(C)]
struct ProcessHeapEntry {
    data: usize,
    size: u32,
    overhead: u8,
    region_index: u8,
    flags: u16,
    union: [usize; 3],
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn HeapLock(heap: *mut c_void) -> i32;
    fn HeapUnlock(heap: *mut c_void) -> i32;
    fn HeapWalk(heap: *mut c_void, entry: *mut ProcessHeapEntry) -> i32;
}

fn allocation_base(address: usize) -> usize {
    let mut info: MemoryBasicInformation = unsafe { std::mem::zeroed() };
    if unsafe { VirtualQuery(address, &mut info, size_of::<MemoryBasicInformation>()) } == 0 {
        return 0;
    }
    info.allocation_base
}

/// The reservations that belong to the default process heap (its segments, and the blocks it
/// got straight from VirtualAlloc), plus the bytes in its free blocks.
fn heap_reservations() -> (Vec<usize>, usize) {
    let mut addresses: Vec<usize> = Vec::with_capacity(1 << 16);
    let mut free = 0usize;
    let heap = unsafe { GetProcessHeap() };
    unsafe {
        HeapLock(heap);
        let mut entry: ProcessHeapEntry = std::mem::zeroed();
        // Every reservation that holds any heap entry (segment heaps do not report all of their
        // memory as regions). The range of the last reservation seen avoids a query per entry.
        let mut current = (0usize, 0usize);
        while HeapWalk(heap, &mut entry) != 0 {
            // Neither region, uncommitted range nor busy block: a free block.
            if entry.flags & 0x7 == 0 {
                free += entry.size as usize;
            }
            if entry.data == 0 || (current.0 <= entry.data && entry.data < current.1) {
                continue;
            }
            let mut info: MemoryBasicInformation = std::mem::zeroed();
            if VirtualQuery(entry.data, &mut info, size_of::<MemoryBasicInformation>()) != 0 {
                current = (info.base, info.base + info.region_size);
            }
            // No allocation while the heap is locked and being walked.
            if addresses.len() < addresses.capacity() {
                addresses.push(entry.data);
            }
        }
        HeapUnlock(heap);
    }
    let mut bases: Vec<usize> = addresses.into_iter().map(allocation_base).collect();
    bases.sort_unstable();
    bases.dedup();
    (bases, free)
}

struct Census {
    totals: [[usize; 3]; 9],
    heap_free: usize,
    /// Reservations of 1 MB or more, and unattributed ones: base, reserved, committed, resident,
    /// zero, class, locked.
    other: Vec<[usize; 7]>,
}

/// All private committed memory of the process (as audit2.py sees it from outside), split by
/// owner where it can be told: committed, resident and resident-but-all-zero bytes.
fn census(own: [usize; 3], sampled_virtual: &[usize]) -> Census {
    let (heap, heap_free) = heap_reservations();
    let mut sampled: Vec<usize> = sampled_virtual.iter().map(|a| allocation_base(*a)).collect();
    sampled.sort_unstable();
    let mut regions = Vec::new();
    let mut address = 0usize;
    let mut info: MemoryBasicInformation = unsafe { std::mem::zeroed() };
    while unsafe { VirtualQuery(address, &mut info, size_of::<MemoryBasicInformation>()) } != 0 {
        if info.state != 0x10000 {
            regions.push((info.base, info.allocation_base, info.region_size, info.state, info.protect, info.kind));
        }
        address = info.base + info.region_size;
        if address >= 0x7FFF_FFFF_0000 {
            break;
        }
    }
    // Reserved span of each allocation, and whether it has a guard page (thread stacks do).
    let mut spans: HashMap<usize, (usize, bool)> = HashMap::new();
    for region in &regions {
        let span = spans.entry(region.1).or_default();
        span.0 += region.2;
        span.1 |= region.3 == 0x1000 && region.4 & 0x100 != 0;
    }
    let mut totals = [[0usize; 3]; 9];
    let mut other: HashMap<usize, [usize; 7]> = HashMap::new();
    let mut info = [WorkingSetExInfo { address: null_mut(), attributes: 0 }; 512];
    for &(base, allocation, size, state, protect, kind) in &regions {
        // Committed, private, not guard / no-access.
        if state != 0x1000 || kind != 0x20000 || protect & 0x101 != 0 {
            continue;
        }
        let (span, guarded) = spans[&allocation];
        let class = if heap.binary_search(&allocation).is_ok() {
            0
        } else if guarded {
            1
        } else if sampled.binary_search(&allocation).is_ok() {
            2
        } else if span == 1 << 20 && allocation % (1 << 20) == 0 {
            3
        } else if own.contains(&allocation) {
            4
        } else if protect & 0xF0 != 0 {
            8
        } else if span <= 1 << 20 {
            5
        } else if span <= 16 << 20 {
            6
        } else {
            7
        };
        let (mut resident, mut zero, mut locked) = (0, 0, 0);
        let mut page = base;
        while page < base + size {
            let n = ((base + size - page) / PAGE).min(info.len());
            for (i, item) in info[..n].iter_mut().enumerate() {
                *item = WorkingSetExInfo { address: (page + i * PAGE) as *mut c_void, attributes: 0 };
            }
            let ok = unsafe {
                K32QueryWorkingSetEx(
                    GetCurrentProcess(),
                    info.as_mut_ptr().cast(),
                    (n * size_of::<WorkingSetExInfo>()) as u32,
                )
            };
            if ok != 0 {
                // Valid and not shared.
                for item in info[..n].iter().filter(|i| i.attributes & 1 != 0 && i.attributes & (1 << 15) == 0) {
                    resident += PAGE;
                    // Locked in memory (as pages pinned for GPU access are).
                    if item.attributes & (1 << 22) != 0 {
                        locked += PAGE;
                    }
                    if page_is_zero(item.address as usize) {
                        zero += PAGE;
                    }
                }
            }
            page += n * PAGE;
        }
        totals[class][0] += size;
        totals[class][1] += resident;
        totals[class][2] += zero;
        if span >= 1 << 20 || class >= 5 {
            let o = other.entry(allocation).or_insert([allocation, span, 0, 0, 0, class, 0]);
            o[2] += size;
            o[3] += resident;
            o[4] += zero;
            o[6] += locked;
        }
    }
    let mut other: Vec<[usize; 7]> = other.into_values().collect();
    other.sort_by(|a, b| b[3].cmp(&a[3]));
    Census { totals, heap_free, other }
}

#[derive(Default)]
struct Aggregate {
    bytes: f64,
    samples: u64,
    raw: u64,
    paged: f64,
    resident: f64,
    zero: f64,
}

fn dump(dir: &Path, label: &str, snapshot: &mut Vec<Sample>) -> std::io::Result<()> {
    let started = std::time::Instant::now();
    snapshot.clear();
    let (stacks_used, arena_len, dropped, stack_slots, arena, own);
    {
        // Every block in the table is live while we hold the lock: frees wait for it.
        let tables = lock();
        for entry in tables.entries().iter() {
            if entry.ptr != 0 && snapshot.len() < snapshot.capacity() {
                let (paged, resident, zero) = scan(entry.ptr, entry.size);
                snapshot.push(Sample { entry: *entry, paged, resident, zero });
            }
        }
        stacks_used = tables.stack_count;
        arena_len = tables.arena_len;
        dropped = tables.dropped;
        stack_slots = tables.stacks as *const StackSlot;
        arena = tables.arena as *const usize;
        own = [tables.entries as usize, tables.stacks as usize, tables.arena as usize];
    }
    // Interned stacks are never changed or removed, so the ones referenced by the snapshot can
    // be read without the lock (which must not be held while this thread allocates or frees).
    let stack = |id: u32| -> &'static [usize] {
        let slot = unsafe { *stack_slots.add(id as usize) };
        unsafe { std::slice::from_raw_parts(arena.add(slot.offset as usize), slot.len as usize) }
    };
    let scan_ms = started.elapsed().as_millis();

    let process = unsafe { GetCurrentProcess() };
    let mut counters: ProcessMemoryCounters = unsafe { std::mem::zeroed() };
    counters.cb = size_of::<ProcessMemoryCounters>() as u32;
    unsafe { K32GetProcessMemoryInfo(process, &mut counters, counters.cb) };

    let mut heaps = [null_mut::<c_void>(); 256];
    let heap_count = (unsafe { GetProcessHeaps(heaps.len() as u32, heaps.as_mut_ptr()) } as usize)
        .min(heaps.len());
    let default_heap = unsafe { GetProcessHeap() };

    let mut aggregates: HashMap<(u32, u8), Aggregate> = HashMap::new();
    let census_started = std::time::Instant::now();
    let sampled_virtual: Vec<usize> = snapshot
        .iter()
        .filter(|s| s.entry.origin == ORIGIN_VIRTUAL)
        .map(|s| s.entry.ptr)
        .collect();
    let census = census(own, &sampled_virtual);
    let census_ms = census_started.elapsed().as_millis();
    // Raw commits in reservations that nothing else accounts for name their owners; the rest
    // duplicate other samples.
    let unattributed: Vec<usize> = {
        let mut bases: Vec<usize> = census.other.iter().filter(|r| r[5] >= 5).map(|r| r[0]).collect();
        bases.sort_unstable();
        bases
    };
    let mut by_origin = [0f64; 6];
    for sample in snapshot.iter() {
        let mut e = sample.entry;
        if e.origin == ORIGIN_RAW && unattributed.binary_search(&allocation_base(e.ptr)).is_err() {
            e.origin = ORIGIN_RAW_DUPLICATE;
        }
        let scale = e.weight / e.size.max(1) as f64;
        let a = aggregates.entry((e.stack, e.origin)).or_default();
        a.bytes += e.weight;
        a.samples += 1;
        a.raw += e.size as u64;
        a.paged += sample.paged as f64 * scale;
        a.resident += sample.resident as f64 * scale;
        a.zero += sample.zero as f64 * scale;
        by_origin[e.origin as usize] += e.weight;
    }

    let temporary = dir.join(format!("dump-{label}.json.tmp"));
    let mut out = std::io::BufWriter::new(std::fs::File::create(&temporary)?);
    let pid = unsafe { GetCurrentProcessId() };
    let overhead = TABLE_CAP * size_of::<Entry>() +
        STACK_CAP * size_of::<StackSlot>() +
        arena_len * 8 +
        snapshot.capacity() * size_of::<Sample>();
    writeln!(out, "{{\"name\": {:?}, \"pid\": {pid}, \"mean_interval\": {},", label, mean())?;
    writeln!(
        out,
        "\"process\": {{\"private_bytes\": {}, \"private_working_set\": {}, \"working_set\": {}, \"peak_working_set\": {}}},",
        counters.private_usage,
        counters.private_working_set_size,
        counters.working_set_size,
        counters.peak_working_set_size
    )?;
    write!(out, "\"heaps\": [")?;
    for (i, heap) in heaps[..heap_count].iter().enumerate() {
        let mut summary: HeapSummaryInfo = unsafe { std::mem::zeroed() };
        summary.cb = size_of::<HeapSummaryInfo>() as u32;
        let ok = unsafe { HeapSummary(*heap, 0, &mut summary) } != 0;
        write!(
            out,
            "{}{{\"handle\": {}, \"default\": {}, \"ok\": {ok}, \"allocated\": {}, \"committed\": {}, \"reserved\": {}}}",
            if i > 0 { ", " } else { "" },
            *heap as usize,
            *heap == default_heap,
            summary.allocated,
            summary.committed,
            summary.reserved
        )?;
    }
    writeln!(out, "],")?;
    writeln!(
        out,
        "\"tracked\": {{\"rust_bytes\": {:.0}, \"crt_bytes\": {:.0}, \"heap_bytes\": {:.0}, \"virtual_bytes\": {:.0}, \"raw_unattributed_bytes\": {:.0}, \"raw_duplicate_bytes\": {:.0}, \"raw_detour\": {}, \"live_samples\": {}, \"dropped_samples\": {dropped}, \"stacks\": {stacks_used}, \"imports_hooked\": {}, \"dlls_hooked\": {}, \"profiler_overhead_bytes\": {overhead}, \"scan_ms\": {scan_ms}, \"census_ms\": {census_ms}}},",
        by_origin[0],
        by_origin[1],
        by_origin[2],
        by_origin[3],
        by_origin[4],
        by_origin[5],
        RAW_DETOURED.load(Relaxed) != 0,
        snapshot.len(),
        PATCHED_IMPORTS.load(Relaxed),
        HOOKED_MODULES.load(Relaxed)
    )?;
    write!(out, "\"census\": [")?;
    for (i, (name, [committed, resident, zero])) in CENSUS_CLASSES.iter().zip(census.totals).enumerate() {
        write!(
            out,
            "{}{{\"class\": {name:?}, \"committed\": {committed}, \"resident\": {resident}, \"zero\": {zero}}}",
            if i > 0 { ", " } else { "" }
        )?;
    }
    writeln!(out, "],")?;
    writeln!(out, "\"heap_free_bytes\": {},", census.heap_free)?;
    writeln!(out, "\"reservations\": {:?},", census.other)?;

    write!(out, "\"modules\": [")?;
    let mut modules = [null_mut::<c_void>(); 1024];
    let mut needed = 0u32;
    unsafe {
        K32EnumProcessModules(
            process,
            modules.as_mut_ptr(),
            (modules.len() * 8) as u32,
            &mut needed,
        )
    };
    let module_count = (needed as usize / 8).min(modules.len());
    for (i, module) in modules[..module_count].iter().enumerate() {
        let mut info: ModuleInfo = unsafe { std::mem::zeroed() };
        unsafe { K32GetModuleInformation(process, *module, &mut info, size_of::<ModuleInfo>() as u32) };
        let mut name = [0u16; 1024];
        let len = unsafe { K32GetModuleFileNameExW(process, *module, name.as_mut_ptr(), 1024) };
        let name = String::from_utf16_lossy(&name[..len as usize]);
        write!(
            out,
            "{}[{}, {}, {:?}]",
            if i > 0 { ",\n " } else { "" },
            info.base as usize,
            info.size,
            name
        )?;
    }
    writeln!(out, "],")?;

    let mut sorted: Vec<_> = aggregates.into_iter().collect();
    sorted.sort_by(|a, b| b.1.bytes.total_cmp(&a.1.bytes));
    writeln!(out, "\"stacks\": [")?;
    for (i, ((id, origin), a)) in sorted.iter().enumerate() {
        let frames = stack(*id);
        write!(
            out,
            "{}{{\"id\": {id}, \"origin\": \"{}\", \"bytes\": {:.0}, \"samples\": {}, \"sampled_bytes\": {}, \"paged\": {:.0}, \"resident\": {:.0}, \"zero\": {:.0}, \"frames\": {:?}}}",
            if i > 0 { ",\n" } else { "" },
            ORIGINS[*origin as usize],
            a.bytes,
            a.samples,
            a.raw,
            a.paged,
            a.resident,
            a.zero,
            frames
        )?;
    }
    writeln!(out, "\n], \"dump_ms\": {}}}", started.elapsed().as_millis())?;
    out.flush()?;
    drop(out);
    std::fs::rename(&temporary, dir.join(format!("dump-{label}.json")))
}
