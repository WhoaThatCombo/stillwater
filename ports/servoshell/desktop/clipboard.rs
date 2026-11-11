/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Every clipboard read and write in Stillwater: the shell's own (sidebar and find bar
//! menus, egui text fields, saved passwords, Copy image/GIF) and the pages' (through
//! [`ShellClipboard`], the engine's clipboard delegate for every WebView).
//!
//! Tests never touch the operating system's clipboard. Under `STILLWATER_NO_OS_CLIPBOARD`
//! (set by `tools/sitecheck/driver.py`), `STILLWATER_CLIPBOARD_TEST_LOG` or the engine pref
//! `dom_clipboard_test_mode`, a process-local stand-in holds the text and HTML instead:
//! reads see what was last written in this process (`STILLWATER_CLIPBOARD_TEST_TEXT` gives it
//! a starting text), and each write is appended as a JSON line to the file named by
//! `STILLWATER_CLIPBOARD_TEST_LOG`, if set.

use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use serde_json::{Value, json};
use servo::{ClipboardDelegate, StringRequest, WebView};

#[derive(Default)]
struct Stand {
    text: String,
    html: Option<String>,
}

fn stand() -> &'static Mutex<Stand> {
    static STAND: OnceLock<Mutex<Stand>> = OnceLock::new();
    STAND.get_or_init(|| {
        Mutex::new(Stand {
            text: std::env::var("STILLWATER_CLIPBOARD_TEST_TEXT").unwrap_or_default(),
            html: None,
        })
    })
}

fn set_stand(text: &str, html: Option<&str>) {
    let mut stand = stand().lock().unwrap_or_else(|poison| poison.into_inner());
    stand.text = text.to_owned();
    stand.html = html.map(str::to_owned);
}

/// Whether the operating system's clipboard is off limits (tests).
pub(crate) fn is_stand_in() -> bool {
    static ENV: OnceLock<bool> = OnceLock::new();
    *ENV.get_or_init(|| {
        std::env::var_os("STILLWATER_NO_OS_CLIPBOARD").is_some() ||
            std::env::var_os("STILLWATER_CLIPBOARD_TEST_LOG").is_some()
    }) || servo::pref!(dom_clipboard_test_mode)
}

/// Tests: append one JSON line describing a write to `STILLWATER_CLIPBOARD_TEST_LOG`.
fn log(entry: Value) -> Result<(), String> {
    let Some(path) = std::env::var_os("STILLWATER_CLIPBOARD_TEST_LOG") else {
        return Ok(());
    };
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut file| writeln!(file, "{entry}"))
        .map_err(|error| error.to_string())
}

fn os() -> Result<arboard::Clipboard, String> {
    debug_assert!(!is_stand_in());
    arboard::Clipboard::new().map_err(|error| error.to_string())
}

/// The clipboard's text, if it has any.
pub(crate) fn get_text() -> Option<String> {
    if is_stand_in() {
        let text = stand()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .text
            .clone();
        return Some(text).filter(|text| !text.is_empty());
    }
    os().ok()?.get_text().ok().filter(|text| !text.is_empty())
}

/// Put plain text on the clipboard. `source` says who wrote it, for the test log.
pub(crate) fn set_text(text: &str, source: &str) {
    if is_stand_in() {
        set_stand(text, None);
        let _ = log(json!({"kind": "text", "source": source, "text": text}));
        return;
    }
    if let Ok(mut clipboard) = os() {
        let _ = clipboard.set_text(text.to_owned());
    }
}

/// Put formatted text on the clipboard: `html` for applications that paste formatting and
/// `text` for the rest.
pub(crate) fn set_html(text: &str, html: &str, source: &str) {
    if is_stand_in() {
        set_stand(text, Some(html));
        let _ = log(json!({"kind": "html", "source": source, "text": text, "html": html}));
        return;
    }
    let Ok(mut clipboard) = os() else {
        return;
    };
    if clipboard.set_html(html, Some(text)).is_err() {
        let _ = clipboard.set_text(text.to_owned());
    }
}

/// Put a username or password on the clipboard, kept out of Windows' clipboard history and
/// cloud clipboard.
pub(crate) fn set_secret(text: &str, source: &str) {
    if is_stand_in() {
        set_stand(text, None);
        let _ = log(json!({"kind": "secret", "source": source, "text": text}));
        return;
    }
    let Ok(mut clipboard) = os() else {
        return;
    };
    #[cfg(windows)]
    {
        use arboard::SetExtWindows;
        let _ = clipboard
            .set()
            .exclude_from_history()
            .exclude_from_cloud()
            .text(text.to_owned());
    }
    #[cfg(not(windows))]
    let _ = clipboard.set_text(text.to_owned());
}

pub(crate) fn clear(source: &str) {
    if is_stand_in() {
        set_stand("", None);
        let _ = log(json!({"kind": "clear", "source": source}));
        return;
    }
    if let Ok(mut clipboard) = os() {
        let _ = clipboard.clear();
    }
}

/// Copy image: the picture as a bitmap (CF_DIBV5 and PNG).
pub(crate) fn set_image(width: u32, height: u32, rgba: Vec<u8>) -> Result<(), String> {
    if is_stand_in() {
        set_stand("", None);
        return log(json!({
            "kind": "image", "formats": ["CF_DIBV5", "PNG"],
            "width": width, "height": height, "bytes": rgba.len(),
        }));
    }
    os()?
        .set_image(arboard::ImageData {
            width: width as usize,
            height: height as usize,
            bytes: rgba.into(),
        })
        .map_err(|error| error.to_string())
}

/// Copy GIF: files, so pasting copies them.
pub(crate) fn set_files(paths: &[PathBuf]) -> Result<(), String> {
    if is_stand_in() {
        set_stand("", None);
        return log(json!({
            "kind": "files", "formats": ["CF_HDROP", "Preferred DropEffect"],
            "paths": paths.iter().map(|path| path.to_string_lossy()).collect::<Vec<_>>(),
        }));
    }
    set_files_on_os_clipboard(paths)
}

/// CF_HDROP with the files, and "Preferred DropEffect" = copy, so pasting copies them.
#[cfg(target_os = "windows")]
fn set_files_on_os_clipboard(paths: &[PathBuf]) -> Result<(), String> {
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
        if ok {
            Ok(())
        } else {
            Err("the clipboard refused the file".into())
        }
    }
}

#[cfg(not(target_os = "windows"))]
fn set_files_on_os_clipboard(paths: &[PathBuf]) -> Result<(), String> {
    os()?
        .set()
        .file_list(paths)
        .map_err(|error| error.to_string())
}

/// The engine's clipboard delegate: what pages copy (Ctrl+C, the context menu,
/// `navigator.clipboard`, `execCommand('copy')`) and what pasting reads.
pub(crate) struct ShellClipboard;

impl ClipboardDelegate for ShellClipboard {
    fn clear(&self, _webview: WebView) {
        clear("page");
    }

    fn get_text(&self, _webview: WebView, request: StringRequest) {
        request.success(get_text().unwrap_or_default());
    }

    fn set_text(&self, _webview: WebView, new_contents: String) {
        set_text(&new_contents, "page");
    }

    fn set_html(&self, _webview: WebView, text: String, html: String) {
        set_html(&text, &html, "page");
    }
}

pub(crate) fn delegate() -> std::rc::Rc<dyn ClipboardDelegate> {
    std::rc::Rc::new(ShellClipboard)
}

/// egui's text fields (find bar, dialogs) copy through egui-winit, which writes the OS
/// clipboard itself; with the stand-in, their copies are taken out of egui's output here
/// first. Call at the end of each egui pass.
pub(crate) fn take_egui_copies(ctx: &egui::Context) {
    if !is_stand_in() {
        return;
    }
    let copies: Vec<String> = ctx.output_mut(|output| {
        let mut copies = Vec::new();
        output.commands.retain(|command| match command {
            egui::OutputCommand::CopyText(text) => {
                copies.push(text.clone());
                false
            },
            egui::OutputCommand::CopyImage(_) => false,
            _ => true,
        });
        copies
    });
    for text in copies {
        set_text(&text, "egui");
    }
}

/// egui-winit reads the OS clipboard itself for Ctrl+V / Shift+Insert / Paste in egui's text
/// fields. With the stand-in, that key press is answered here instead: returns the paste
/// event to give egui, or `None` to let egui-winit handle the event.
pub(crate) fn egui_paste_instead(
    event: &winit::event::WindowEvent,
    modifiers: egui::Modifiers,
) -> Option<Option<egui::Event>> {
    use winit::keyboard::{Key, NamedKey};
    if !is_stand_in() {
        return None;
    }
    let winit::event::WindowEvent::KeyboardInput { event, .. } = event else {
        return None;
    };
    if event.state != winit::event::ElementState::Pressed {
        return None;
    }
    let is_paste = match &event.logical_key {
        Key::Named(NamedKey::Paste) => true,
        Key::Named(NamedKey::Insert) => modifiers.shift,
        Key::Character(character) => modifiers.command && character.eq_ignore_ascii_case("v"),
        _ => false,
    };
    if !is_paste {
        return None;
    }
    Some(
        get_text()
            .map(|text| text.replace("\r\n", "\n"))
            .map(egui::Event::Paste),
    )
}
