/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! "Copy image" and "Copy GIF" in the page's context menu.
//!
//! The file is fetched again through Servo's network stack (the page's cookies and HTTP cache,
//! as a download that never shows in the Downloads panel) into Stillwater's clip folder, then
//! turned into clipboard contents on a worker thread:
//! - Copy image: the decoded picture as a bitmap (CF_DIBV5 and PNG, via arboard).
//! - Copy GIF: a .gif file on the clipboard as a file (CF_HDROP, "Preferred DropEffect" copy),
//!   so it pastes as a file into chat apps and Explorer. A GIF image is used as it is; a video
//!   shown like a GIF (X's looping muted mp4s) is decoded with GStreamer (the media backend's
//!   own plugins) and encoded as a GIF: at most 15 s, 480 px wide and 25 frames a second,
//!   looping forever.
//!
//! The clip folder (`%TEMP%\Stillwater\clips`, or `STILLWATER_CLIP_DIR`) is emptied at startup.
//! `STILLWATER_CLIPBOARD_TEST_LOG=<file>`: tests get a JSON line per clipboard write in that
//! file instead of the clipboard being touched.

use std::cell::RefCell;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use servo::{DownloadId, DownloadRequest, DownloadUpdate, Servo, WebViewId};

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Kind {
    Image,
    Gif,
}

/// Longest part of a video made into a GIF, its widest size and its frame interval.
const GIF_MAX_DURATION_MS: u64 = 15_000;
const GIF_MAX_WIDTH: u32 = 480;
const GIF_MIN_FRAME_MS: u64 = 40;
/// How long "Image copied" and friends stay in the status line.
const NOTICE_TIME: Duration = Duration::from_secs(3);

struct Pending {
    url: url::Url,
    kind: Kind,
    asked: Instant,
}

thread_local! {
    /// Fetches asked for whose download request hasn't arrived yet.
    static PENDING: RefCell<Vec<Pending>> = const { RefCell::new(Vec::new()) };
    /// Fetches being written: download id, kind, file, URL.
    static ACTIVE: RefCell<Vec<(DownloadId, Kind, PathBuf, url::Url)>> = const { RefCell::new(Vec::new()) };
}

/// The status-line message and when it goes away (`None`: while the work lasts).
static NOTICE: Mutex<Option<(String, Option<Instant>)>> = Mutex::new(None);
static CHANGED: AtomicBool = AtomicBool::new(false);
static WORKERS: AtomicUsize = AtomicUsize::new(0);
static NEXT_FILE: AtomicUsize = AtomicUsize::new(0);

/// Show `text` in the status line (for `lasts`, or until replaced).
pub(crate) fn set_notice(text: impl Into<String>, lasts: Option<Duration>) {
    if let Ok(mut notice) = NOTICE.lock() {
        *notice = Some((text.into(), lasts.map(|lasts| Instant::now() + lasts)));
    }
    CHANGED.store(true, Ordering::Relaxed);
}

/// What the status line shows for a copy in progress or just done.
pub(crate) fn notice() -> Option<String> {
    let mut notice = NOTICE.lock().ok()?;
    if notice
        .as_ref()
        .is_some_and(|(_, until)| until.is_some_and(|until| Instant::now() >= until))
    {
        *notice = None;
        CHANGED.store(true, Ordering::Relaxed);
    }
    notice.as_ref().map(|(text, _)| text.clone())
}

/// Whether the event loop should keep ticking (work in progress or a message showing).
pub(crate) fn busy() -> bool {
    WORKERS.load(Ordering::Relaxed) > 0 ||
        PENDING.with(|pending| !pending.borrow().is_empty()) ||
        ACTIVE.with(|active| !active.borrow().is_empty()) ||
        NOTICE.lock().is_ok_and(|notice| notice.is_some())
}

/// Whether the status line needs refreshing since the last call.
pub(crate) fn take_changed() -> bool {
    let _ = notice();
    CHANGED.swap(false, Ordering::Relaxed)
}

fn clip_dir() -> PathBuf {
    std::env::var_os("STILLWATER_CLIP_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("Stillwater").join("clips"))
}

/// Remove the files earlier copies left (they were only needed until pasted).
pub(crate) fn clean_up() {
    let Ok(entries) = std::fs::read_dir(clip_dir()) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let ours = path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| matches!(extension, "gif" | "part"));
        if ours && path.is_file() {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// The user picked "Copy image" or "Copy GIF" for `url` in `webview`.
pub(crate) fn start(servo: &Servo, webview: WebViewId, kind: Kind, url: url::Url) {
    crate::desktop::diagnostics::log(format!("copy {kind:?}: fetching {}", url.as_str()));
    PENDING.with(|pending| {
        let mut pending = pending.borrow_mut();
        pending.retain(|item| item.asked.elapsed() < Duration::from_secs(60));
        pending.push(Pending {
            url: url.clone(),
            kind,
            asked: Instant::now(),
        });
    });
    set_notice(
        match kind {
            Kind::Image => "Copying image…",
            Kind::Gif => "Copying GIF…",
        },
        None,
    );
    servo.network_manager().download(webview, url);
}

/// A download Servo wants to save: if it is one of ours, save it to the clip folder and
/// return `None`; otherwise hand it back.
pub(crate) fn claim(request: DownloadRequest) -> Option<DownloadRequest> {
    let url = request.url();
    let picked = PENDING.with(|pending| {
        let mut pending = pending.borrow_mut();
        pending.retain(|item| item.asked.elapsed() < Duration::from_secs(60));
        let index = pending
            .iter()
            .position(|item| item.url == url)
            // Redirected: the only fetch we're waiting for.
            .or_else(|| (pending.len() == 1).then_some(0))?;
        Some(pending.remove(index))
    });
    let Some(picked) = picked else {
        return Some(request);
    };
    let directory = clip_dir();
    if let Err(error) = std::fs::create_dir_all(&directory) {
        failed(picked.kind, &format!("no clip folder ({error})"));
        request.cancel();
        return None;
    }
    let number = NEXT_FILE.fetch_add(1, Ordering::Relaxed);
    let path = directory.join(format!("fetch-{}-{number}.part", std::process::id()));
    let id = request.id();
    let _ = request.accept(path.clone());
    ACTIVE.with(|active| active.borrow_mut().push((id, picked.kind, path, picked.url)));
    None
}

/// Progress of a download; returns true if it was one of ours.
pub(crate) fn update(id: DownloadId, update: &DownloadUpdate) -> bool {
    let entry = ACTIVE.with(|active| {
        let mut active = active.borrow_mut();
        let index = active.iter().position(|(other, ..)| *other == id)?;
        if matches!(update, DownloadUpdate::Progress { .. }) {
            return Some(None);
        }
        Some(Some(active.remove(index)))
    });
    let Some(entry) = entry else {
        return false;
    };
    let Some((_, kind, path, url)) = entry else {
        return true;
    };
    match update {
        DownloadUpdate::Finished { .. } => {
            WORKERS.fetch_add(1, Ordering::Relaxed);
            let spawned = std::thread::Builder::new()
                .name("Copy image".into())
                .spawn(move || {
                    let result = process(kind, &path, &url);
                    let _ = std::fs::remove_file(&path);
                    match result {
                        Ok(message) => {
                            crate::desktop::diagnostics::log(format!("copy {kind:?}: {message}"));
                            set_notice(message, Some(NOTICE_TIME));
                        },
                        Err(error) => failed(kind, &error),
                    }
                    WORKERS.fetch_sub(1, Ordering::Relaxed);
                    CHANGED.store(true, Ordering::Relaxed);
                });
            if spawned.is_err() {
                WORKERS.fetch_sub(1, Ordering::Relaxed);
                failed(kind, "could not start");
            }
        },
        DownloadUpdate::Failed(error) => failed(kind, error),
        DownloadUpdate::Cancelled => failed(kind, "cancelled"),
        DownloadUpdate::Progress { .. } => {},
    }
    true
}

fn failed(kind: Kind, error: &str) {
    crate::desktop::diagnostics::log(format!("copy {kind:?} failed: {error}"));
    set_notice(
        match kind {
            Kind::Image => "Couldn't copy the image",
            Kind::Gif => "Couldn't copy the GIF",
        },
        Some(NOTICE_TIME),
    );
}

fn process(kind: Kind, path: &Path, url: &url::Url) -> Result<String, String> {
    let bytes = std::fs::read(path).map_err(|error| error.to_string())?;
    match kind {
        Kind::Image => {
            let image = image::load_from_memory(&bytes)
                .map_err(|error| format!("not an image Stillwater can read ({error})"))?
                .to_rgba8();
            let (width, height) = image.dimensions();
            clipboard::set_image(width, height, image.into_raw())?;
            Ok("Image copied".into())
        },
        Kind::Gif => {
            let target = gif_path(url);
            if bytes.starts_with(b"GIF8") {
                // A GIF already: copied as it is.
                std::fs::write(&target, &bytes).map_err(|error| error.to_string())?;
            } else {
                video_to_gif(path, &target)?;
            }
            clipboard::set_files(&[target])?;
            Ok("GIF copied".into())
        },
    }
}

/// A free name in the clip folder from the URL's file name, e.g. `tweet_video-AbC.gif`.
fn gif_path(url: &url::Url) -> PathBuf {
    let stem: String = url
        .path_segments()
        .and_then(|mut segments| segments.next_back())
        .map(|name| name.rsplit_once('.').map_or(name, |(stem, _)| stem))
        .unwrap_or_default()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        .take(40)
        .collect();
    let stem = if stem.is_empty() { "stillwater".to_owned() } else { stem };
    let directory = clip_dir();
    let mut path = directory.join(format!("{stem}.gif"));
    let mut number = 1;
    while path.exists() {
        number += 1;
        path = directory.join(format!("{stem}-{number}.gif"));
    }
    path
}

/// Decode the video at `source` and write it as a looping GIF to `target`.
#[cfg(feature = "media-gstreamer")]
fn video_to_gif(source: &Path, target: &Path) -> Result<(), String> {
    use gstreamer as gst;
    use gstreamer::prelude::*;
    use image::codecs::gif::{GifEncoder, Repeat};
    use image::{Delay, Frame, RgbaImage};

    gst::init().map_err(|error| error.to_string())?;
    let make = |name: &str| {
        gst::ElementFactory::make(name)
            .build()
            .map_err(|_| format!("the {name} media element is missing"))
    };
    let pipeline = gst::Pipeline::new();
    let filesrc = make("filesrc")?;
    filesrc.set_property("location", source.to_string_lossy().as_ref());
    let decode = make("decodebin")?;
    let convert = make("videoconvert")?;
    let sink = gstreamer_app::AppSink::builder()
        .caps(&gst::Caps::builder("video/x-raw").field("format", "RGBA").build())
        .sync(false)
        .build();
    pipeline
        .add_many([&filesrc, &decode, &convert, sink.upcast_ref()])
        .map_err(|error| error.to_string())?;
    filesrc.link(&decode).map_err(|error| error.to_string())?;
    convert.link(&sink).map_err(|error| error.to_string())?;
    let convert_weak = convert.downgrade();
    let pipeline_weak = pipeline.downgrade();
    decode.connect_pad_added(move |_, pad| {
        let is_video = pad
            .current_caps()
            .and_then(|caps| caps.structure(0).map(|s| s.name().starts_with("video/")))
            .unwrap_or(false);
        if is_video {
            if let Some(sink_pad) = convert_weak.upgrade().and_then(|c| c.static_pad("sink")) &&
                !sink_pad.is_linked()
            {
                let _ = pad.link(&sink_pad);
            }
        } else if let Some(pipeline) = pipeline_weak.upgrade() &&
            let Ok(fake) = gst::ElementFactory::make("fakesink").property("sync", false).build()
        {
            // Sound and other streams go nowhere.
            let _ = pipeline.add(&fake);
            let _ = fake.sync_state_with_parent();
            if let Some(fake_pad) = fake.static_pad("sink") {
                let _ = pad.link(&fake_pad);
            }
        }
    });

    let result = (|| {
        pipeline
            .set_state(gst::State::Playing)
            .map_err(|_| "the video could not be opened".to_owned())?;
        let file = std::fs::File::create(target).map_err(|error| error.to_string())?;
        let mut encoder = GifEncoder::new_with_speed(std::io::BufWriter::new(file), 10);
        encoder
            .set_repeat(Repeat::Infinite)
            .map_err(|error| error.to_string())?;
        // Frames are written one behind, once the next one's time gives this one's length.
        let mut held: Option<(RgbaImage, u64)> = None;
        let mut next_ms = 0;
        let mut frames = 0;
        let mut last_gap = GIF_MIN_FRAME_MS;
        // GIF delays are whole hundredths of a second: each frame ends at its next frame's time
        // rounded, so the rounding doesn't add up and the GIF keeps the video's speed.
        let mut first_ms = None;
        let mut written_cs = 0u64;
        let mut delay_until = |end_ms: u64, first_ms: u64| {
            let end_cs = (end_ms.saturating_sub(first_ms) + 5) / 10;
            let delay = end_cs.saturating_sub(written_cs).max(1);
            written_cs += delay;
            Delay::from_numer_denom_ms(delay as u32 * 10, 1)
        };
        while let Some(sample) = sink.try_pull_sample(gst::ClockTime::from_seconds(20)) {
            let Some(buffer) = sample.buffer() else { continue };
            let time_ms = buffer.pts().map_or(0, |pts| pts.mseconds());
            if time_ms >= GIF_MAX_DURATION_MS {
                break;
            }
            if time_ms < next_ms && held.is_some() {
                continue;
            }
            next_ms = time_ms.max(next_ms) + GIF_MIN_FRAME_MS;
            let Some(caps) = sample.caps() else { continue };
            let Some(structure) = caps.structure(0) else { continue };
            let (Ok(width), Ok(height)) =
                (structure.get::<i32>("width"), structure.get::<i32>("height"))
            else {
                continue;
            };
            let (width, height) = (width as u32, height as u32);
            let Ok(map) = buffer.map_readable() else { continue };
            let stride = map.len() / height.max(1) as usize;
            let row = width as usize * 4;
            if stride < row {
                continue;
            }
            let mut pixels = Vec::with_capacity(row * height as usize);
            for y in 0..height as usize {
                pixels.extend_from_slice(&map[y * stride..y * stride + row]);
            }
            let Some(mut picture) = RgbaImage::from_raw(width, height, pixels) else {
                continue;
            };
            if width > GIF_MAX_WIDTH {
                let new_height = (height as u64 * GIF_MAX_WIDTH as u64 / width as u64).max(1) as u32;
                picture = image::imageops::resize(
                    &picture,
                    GIF_MAX_WIDTH,
                    new_height,
                    image::imageops::FilterType::Triangle,
                );
            }
            let first = *first_ms.get_or_insert(time_ms);
            if let Some((previous, previous_ms)) = held.take() {
                last_gap = time_ms.saturating_sub(previous_ms).max(10);
                let delay = delay_until(time_ms, first);
                encoder
                    .encode_frame(Frame::from_parts(previous, 0, 0, delay))
                    .map_err(|error| error.to_string())?;
                frames += 1;
            }
            held = Some((picture, time_ms));
        }
        if let Some(bus) = pipeline.bus() &&
            let Some(message) = bus.pop_filtered(&[gst::MessageType::Error])
            && let gst::MessageView::Error(error) = message.view()
            && frames == 0
        {
            return Err(format!("the video could not be decoded ({})", error.error()));
        }
        if let Some((previous, previous_ms)) = held.take() {
            let delay = delay_until(previous_ms + last_gap, first_ms.unwrap_or(0));
            encoder
                .encode_frame(Frame::from_parts(previous, 0, 0, delay))
                .map_err(|error| error.to_string())?;
            frames += 1;
        }
        if frames == 0 {
            return Err("the video had no pictures".into());
        }
        Ok(())
    })();
    let _ = pipeline.set_state(gst::State::Null);
    if result.is_err() {
        let _ = std::fs::remove_file(target);
    }
    result
}

#[cfg(not(feature = "media-gstreamer"))]
fn video_to_gif(_source: &Path, _target: &Path) -> Result<(), String> {
    Err("this build has no video decoder".into())
}

mod clipboard {
    use std::path::PathBuf;

    use super::Write;

    /// Tests: describe the clipboard write in this file instead of making it.
    fn test_log(entry: serde_json::Value) -> Option<Result<(), String>> {
        let path = std::env::var_os("STILLWATER_CLIPBOARD_TEST_LOG")?;
        Some(
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .and_then(|mut file| writeln!(file, "{entry}"))
                .map_err(|error| error.to_string()),
        )
    }

    pub(super) fn set_image(width: u32, height: u32, rgba: Vec<u8>) -> Result<(), String> {
        if let Some(result) = test_log(serde_json::json!({
            "kind": "image", "formats": ["CF_DIBV5", "PNG"],
            "width": width, "height": height, "bytes": rgba.len(),
        })) {
            return result;
        }
        let mut clipboard = arboard::Clipboard::new().map_err(|error| error.to_string())?;
        clipboard
            .set_image(arboard::ImageData {
                width: width as usize,
                height: height as usize,
                bytes: rgba.into(),
            })
            .map_err(|error| error.to_string())
    }

    pub(super) fn set_files(paths: &[PathBuf]) -> Result<(), String> {
        if let Some(result) = test_log(serde_json::json!({
            "kind": "files", "formats": ["CF_HDROP", "Preferred DropEffect"],
            "paths": paths.iter().map(|path| path.to_string_lossy()).collect::<Vec<_>>(),
        })) {
            return result;
        }
        set_files_on_clipboard(paths)
    }

    /// CF_HDROP with the files, and "Preferred DropEffect" = copy, so pasting copies them.
    #[cfg(target_os = "windows")]
    fn set_files_on_clipboard(paths: &[PathBuf]) -> Result<(), String> {
        use std::ffi::c_void;
        use std::os::windows::ffi::OsStrExt;

        #[link(name = "user32")]
        unsafe extern "system" {
            fn OpenClipboard(owner: *mut c_void) -> i32;
            fn EmptyClipboard() -> i32;
            fn CloseClipboard() -> i32;
            fn SetClipboardData(format: u32, memory: *mut c_void) -> *mut c_void;
            fn RegisterClipboardFormatW(name: *const u16) -> u32;
        }
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GlobalAlloc(flags: u32, bytes: usize) -> *mut c_void;
            fn GlobalLock(memory: *mut c_void) -> *mut c_void;
            fn GlobalUnlock(memory: *mut c_void) -> i32;
            fn GlobalFree(memory: *mut c_void) -> *mut c_void;
        }
        const CF_HDROP: u32 = 15;
        const GMEM_MOVEABLE: u32 = 2;
        const DROPEFFECT_COPY: u32 = 1;

        // DROPFILES (20 bytes: pFiles, pt.x, pt.y, fNC, fWide), then the wide paths, each
        // ending in a null, and a final null.
        let mut data: Vec<u8> = Vec::new();
        for value in [20u32, 0, 0, 0, 1] {
            data.extend_from_slice(&value.to_le_bytes());
        }
        for path in paths {
            for unit in path.as_os_str().encode_wide().chain([0]) {
                data.extend_from_slice(&unit.to_le_bytes());
            }
        }
        data.extend_from_slice(&[0, 0]);

        // SAFETY: plain Win32 clipboard calls; memory handed to SetClipboardData belongs to the
        // clipboard afterwards and is freed here only if it was refused.
        unsafe {
            let global = |bytes: &[u8]| {
                let memory = GlobalAlloc(GMEM_MOVEABLE, bytes.len());
                if memory.is_null() {
                    return memory;
                }
                let pointer = GlobalLock(memory);
                if pointer.is_null() {
                    GlobalFree(memory);
                    return std::ptr::null_mut();
                }
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), pointer as *mut u8, bytes.len());
                GlobalUnlock(memory);
                memory
            };
            let mut opened = false;
            for _ in 0..10 {
                if OpenClipboard(std::ptr::null_mut()) != 0 {
                    opened = true;
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(30));
            }
            if !opened {
                return Err("the clipboard is busy".into());
            }
            EmptyClipboard();
            let files = global(&data);
            let ok = !files.is_null() && !SetClipboardData(CF_HDROP, files).is_null();
            if !ok && !files.is_null() {
                GlobalFree(files);
            }
            let name: Vec<u16> = "Preferred DropEffect".encode_utf16().chain([0]).collect();
            let format = RegisterClipboardFormatW(name.as_ptr());
            if format != 0 {
                let effect = global(&DROPEFFECT_COPY.to_le_bytes());
                if !effect.is_null() && SetClipboardData(format, effect).is_null() {
                    GlobalFree(effect);
                }
            }
            CloseClipboard();
            if ok { Ok(()) } else { Err("the clipboard refused the file".into()) }
        }
    }

    #[cfg(not(target_os = "windows"))]
    fn set_files_on_clipboard(paths: &[PathBuf]) -> Result<(), String> {
        arboard::Clipboard::new()
            .and_then(|mut clipboard| clipboard.set().file_list(paths))
            .map_err(|error| error.to_string())
    }
}
