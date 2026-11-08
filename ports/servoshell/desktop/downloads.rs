/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Downloads: where files are saved, their progress, and the sidebar's Downloads panel.
//!
//! Servo asks where to save a file ([`servo::WebViewDelegate::request_download`]). It goes to
//! the Windows Downloads folder (or, with "Ask where to save each file", wherever the user
//! picks) under a name that isn't taken yet, written as `<name>.part` and renamed when it's
//! complete. Servo removes the `.part` file if the download fails or is cancelled.
//!
//! The list is kept in `<config dir>/downloads.json`. Downloaded files are never opened
//! automatically.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use log::warn;
use serde_json::{Value, json};
use servo::{DownloadHandle, DownloadId, DownloadRequest, DownloadUpdate, Servo, WebViewId};
use url::Url;
use winit::event_loop::EventLoopProxy;

use crate::desktop::event_loop::AppEvent;
use crate::desktop::settings::config_dir;

/// The list keeps this many finished downloads.
const MAX_ITEMS: usize = 200;

/// File types that can run programs: opening one asks first.
const EXECUTABLE_EXTENSIONS: &[&str] = &[
    "exe",
    "msi",
    "msix",
    "msixbundle",
    "appx",
    "appxbundle",
    "bat",
    "cmd",
    "com",
    "cpl",
    "hta",
    "jar",
    "js",
    "jse",
    "lnk",
    "msp",
    "ps1",
    "psm1",
    "reg",
    "scr",
    "vbe",
    "vbs",
    "wsf",
    "wsh",
];

pub(crate) type SharedDownloads = Rc<RefCell<Downloads>>;

#[derive(Clone, Copy, Debug, PartialEq)]
enum State {
    Downloading,
    Done,
    Failed,
    Cancelled,
}

impl State {
    fn id(self) -> &'static str {
        match self {
            State::Downloading => "downloading",
            State::Done => "done",
            State::Failed => "failed",
            State::Cancelled => "cancelled",
        }
    }

    fn from_id(id: &str) -> Self {
        match id {
            "done" => State::Done,
            "cancelled" => State::Cancelled,
            // A download that was still running when Stillwater closed didn't finish.
            _ => State::Failed,
        }
    }
}

struct Item {
    /// A stable id for the page and for downloads.json.
    key: u64,
    url: String,
    /// Where the file is (or will be, once complete).
    path: PathBuf,
    /// Where the data is written while downloading.
    part_path: PathBuf,
    total: Option<u64>,
    received: u64,
    state: State,
    error: Option<String>,
    /// Seconds since 1970 when the download started.
    started: u64,
    /// Bytes per second, smoothed.
    speed: f64,
    last_sample: Option<(Instant, u64)>,
    /// The download finished but the user is still choosing where to save it.
    finished_early: bool,
    /// The save dialog is open for this download.
    choosing: bool,
    /// The file is no longer where it was saved.
    missing: bool,
}

impl Item {
    fn name(&self) -> String {
        file_name(&self.path)
    }

    fn to_json(&self) -> Value {
        json!({
            "key": self.key,
            "name": self.name(),
            "url": self.url,
            "path": self.path.display().to_string(),
            "state": self.state.id(),
            "received": self.received,
            "total": self.total,
            "speed": self.speed.round() as u64,
            "error": self.error,
            "executable": is_executable(&self.path),
            "missing": self.missing,
            "choosing": self.choosing,
        })
    }

    fn to_saved_json(&self) -> Value {
        json!({
            "key": self.key,
            // Don't keep huge `data:` URLs (pages "export" files that way).
            "url": if self.url.len() > 4096 { "" } else { self.url.as_str() },
            "path": self.path.display().to_string(),
            "partPath": self.part_path.display().to_string(),
            "state": self.state.id(),
            "received": self.received,
            "total": self.total,
            "error": self.error,
            "started": self.started,
        })
    }
}

pub(crate) struct Downloads {
    items: Vec<Item>,
    ask_where_to_save: bool,
    next_key: u64,
    handles: HashMap<u64, DownloadHandle>,
    ids: HashMap<DownloadId, u64>,
    /// Downloads being retried: the URL and the list entry to reuse.
    retries: Vec<(String, u64)>,
    /// Save dialogs that closed: the download and the file chosen, if any.
    dialog_results: Arc<Mutex<Vec<(u64, Option<PathBuf>)>>>,
    /// Wakes the event loop when a save dialog closes.
    event_loop_proxy: Option<EventLoopProxy<AppEvent>>,
    /// The list changed and the sidebar should be updated.
    changed: bool,
    saved_path: Option<PathBuf>,
}

impl Downloads {
    pub(crate) fn load() -> SharedDownloads {
        let saved_path = config_dir().map(|dir| dir.join("downloads.json"));
        let saved = saved_path
            .as_ref()
            .and_then(|path| std::fs::read_to_string(path).ok())
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .unwrap_or(Value::Null);
        let mut next_key = 1;
        let mut leftover = false;
        let items: Vec<Item> = saved["items"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|item| {
                let key = item["key"].as_u64()?;
                next_key = next_key.max(key + 1);
                let path = PathBuf::from(item["path"].as_str()?);
                let part_path = item["partPath"]
                    .as_str()
                    .map(PathBuf::from)
                    .unwrap_or_else(|| part_path_for(&path));
                let mut state = State::from_id(item["state"].as_str().unwrap_or_default());
                let mut error = item["error"].as_str().map(str::to_owned);
                if item["state"] == "downloading" {
                    // Stillwater closed while this was downloading: its partial file is useless.
                    leftover = true;
                    let _ = std::fs::remove_file(&part_path);
                    state = State::Failed;
                    error = Some("Stillwater closed before the download finished.".into());
                }
                Some(Item {
                    key,
                    url: item["url"].as_str().unwrap_or_default().to_owned(),
                    missing: state == State::Done && !path.exists(),
                    path,
                    part_path,
                    total: item["total"].as_u64(),
                    received: item["received"].as_u64().unwrap_or(0),
                    state,
                    error,
                    started: item["started"].as_u64().unwrap_or(0),
                    speed: 0.0,
                    last_sample: None,
                    finished_early: false,
                    choosing: false,
                })
            })
            .collect();
        let downloads = Downloads {
            items,
            ask_where_to_save: saved["askWhereToSave"].as_bool().unwrap_or(false),
            next_key,
            handles: HashMap::new(),
            ids: HashMap::new(),
            retries: Vec::new(),
            dialog_results: Default::default(),
            event_loop_proxy: None,
            changed: true,
            saved_path,
        };
        if leftover {
            downloads.save();
        }
        Rc::new(RefCell::new(downloads))
    }

    fn save(&self) {
        let Some(path) = &self.saved_path else {
            return;
        };
        let items: Vec<Value> = self.items.iter().map(Item::to_saved_json).collect();
        let data = json!({ "askWhereToSave": self.ask_where_to_save, "items": items });
        let result = path
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|_| std::fs::write(path, data.to_string()));
        if let Err(error) = result {
            warn!(
                "Could not save the downloads list to {}: {error}",
                path.display()
            );
        }
    }

    /// Lets save dialogs wake the event loop when they close.
    pub(crate) fn set_event_loop_proxy(&mut self, proxy: EventLoopProxy<AppEvent>) {
        if self.event_loop_proxy.is_none() {
            self.event_loop_proxy = Some(proxy);
        }
    }

    /// Whether the list changed since the last call (the sidebar needs updating).
    pub(crate) fn take_changed(&mut self) -> bool {
        self.poll_dialogs();
        std::mem::take(&mut self.changed)
    }

    fn item_mut(&mut self, key: u64) -> Option<&mut Item> {
        self.items.iter_mut().find(|item| item.key == key)
    }

    /// Paths taken by downloads in progress, which new downloads must not reuse.
    fn reserved(&self, path: &Path) -> bool {
        self.items.iter().any(|item| {
            item.state == State::Downloading && (item.path == path || item.part_path == path)
        })
    }

    /// Servo wants to save a response as a file.
    pub(crate) fn start(&mut self, request: DownloadRequest) {
        let directory = download_dir();
        if let Err(error) = std::fs::create_dir_all(&directory) {
            warn!("Could not create {}: {error}", directory.display());
        }
        let name = sanitize_file_name(request.suggested_filename(), request.mime_type());
        let path = unique_path(&directory, &name, |path| self.reserved(path));
        let part_path = part_path_for(&path);
        let url = request.url().to_string();
        let total = request.total_bytes();
        let id = request.id();
        crate::desktop::diagnostics::log(format!(
            "download started: {} ({})",
            file_name(&path),
            total.map_or("size unknown".to_owned(), |total| format!("{total} bytes"))
        ));
        let handle = request.accept(part_path.clone());

        // Retrying reuses the failed entry.
        let key = match self
            .retries
            .iter()
            .position(|(retry_url, _)| *retry_url == url)
        {
            Some(index) => {
                let (_, key) = self.retries.remove(index);
                self.items.retain(|item| item.key != key);
                key
            },
            None => {
                self.next_key += 1;
                self.next_key
            },
        };
        let started = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|time| time.as_secs())
            .unwrap_or(0);
        self.items.insert(
            0,
            Item {
                key,
                url,
                path,
                part_path,
                total,
                received: 0,
                state: State::Downloading,
                error: None,
                started,
                speed: 0.0,
                last_sample: Some((Instant::now(), 0)),
                finished_early: false,
                choosing: self.ask_where_to_save,
                missing: false,
            },
        );
        self.handles.insert(key, handle);
        self.ids.insert(id, key);
        self.trim();
        if self.ask_where_to_save {
            // The data is saved to the Downloads folder meanwhile, and moved when it's chosen.
            self.ask_for_location(key);
        }
        self.changed = true;
        self.save();
    }

    /// Show a save dialog on its own thread so the window keeps working.
    fn ask_for_location(&self, key: u64) {
        let Some(item) = self.items.iter().find(|item| item.key == key) else {
            return;
        };
        let name = item.name();
        let directory = item.path.parent().map(Path::to_owned);
        let results = self.dialog_results.clone();
        let proxy = self.event_loop_proxy.clone();
        let spawned = std::thread::Builder::new()
            .name("Save dialog".into())
            .spawn(move || {
                let mut dialog = rfd::FileDialog::new()
                    .set_title("Save as")
                    .set_file_name(&name);
                if let Some(directory) = directory {
                    dialog = dialog.set_directory(directory);
                }
                if let Some(extension) = Path::new(&name).extension().and_then(|ext| ext.to_str()) {
                    dialog = dialog.add_filter(extension.to_uppercase(), &[extension]);
                }
                let chosen = dialog.save_file();
                if let Ok(mut results) = results.lock() {
                    results.push((key, chosen));
                }
                if let Some(proxy) = proxy {
                    let _ = proxy.send_event(AppEvent::Waker);
                }
            });
        if let Err(error) = spawned {
            warn!("Could not show the save dialog: {error}");
        }
    }

    /// Act on save dialogs that closed.
    fn poll_dialogs(&mut self) {
        let results = match self.dialog_results.lock() {
            Ok(mut results) => std::mem::take(&mut *results),
            Err(_) => return,
        };
        for (key, chosen) in results {
            let Some(item) = self.item_mut(key) else {
                continue;
            };
            item.choosing = false;
            match chosen {
                Some(path) => {
                    item.path = path;
                    if item.finished_early {
                        self.finish(key);
                    }
                },
                None => {
                    // Cancelled: forget the download.
                    if let Some(handle) = self.handles.remove(&key) {
                        handle.cancel();
                    }
                    if let Some(item) = self.items.iter().find(|item| item.key == key) &&
                        item.finished_early
                    {
                        let _ = std::fs::remove_file(&item.part_path);
                    }
                    self.items.retain(|item| item.key != key);
                },
            }
            self.changed = true;
            self.save();
        }
    }

    /// Progress reported by Servo.
    pub(crate) fn update(&mut self, id: DownloadId, update: DownloadUpdate) {
        let Some(&key) = self.ids.get(&id) else {
            return;
        };
        let Some(index) = self.items.iter().position(|item| item.key == key) else {
            return;
        };
        self.changed = true;
        let ended = !matches!(update, DownloadUpdate::Progress { .. });
        if ended {
            self.ids.remove(&id);
            self.handles.remove(&key);
        }
        let item = &mut self.items[index];
        match update {
            DownloadUpdate::Progress {
                received_bytes,
                total_bytes,
            } => {
                item.received = received_bytes;
                item.total = total_bytes.or(item.total);
                let now = Instant::now();
                if let Some((then, bytes)) = item.last_sample {
                    let seconds = now.duration_since(then).as_secs_f64();
                    if seconds >= 0.2 {
                        let current = received_bytes.saturating_sub(bytes) as f64 / seconds;
                        item.speed = if item.speed == 0.0 {
                            current
                        } else {
                            item.speed * 0.6 + current * 0.4
                        };
                        item.last_sample = Some((now, received_bytes));
                    }
                }
                return;
            },
            DownloadUpdate::Finished { received_bytes } => {
                item.received = received_bytes;
                item.total = Some(received_bytes);
                if item.choosing {
                    item.finished_early = true;
                } else {
                    self.finish(key);
                }
                return;
            },
            DownloadUpdate::Failed(error) => {
                crate::desktop::diagnostics::log(format!("download failed: {error}"));
                item.state = State::Failed;
                item.error = Some(error);
            },
            DownloadUpdate::Cancelled => {
                crate::desktop::diagnostics::log("download cancelled");
                item.state = State::Cancelled;
            },
        }
        item.speed = 0.0;
        self.save();
    }

    /// Move a complete download from its `.part` file to its name.
    fn finish(&mut self, key: u64) {
        let Some(index) = self.items.iter().position(|item| item.key == key) else {
            return;
        };
        // The name may have been taken while downloading.
        let mut path = self.items[index].path.clone();
        if path.exists() && !self.items[index].choosing {
            let directory = path.parent().map(Path::to_owned).unwrap_or_default();
            path = unique_path(&directory, &file_name(&path), |_| false);
        }
        let item = &mut self.items[index];
        item.speed = 0.0;
        match move_file(&item.part_path, &path) {
            Ok(()) => {
                item.path = path;
                item.state = State::Done;
                mark_as_downloaded(&item.path, &item.url);
                crate::desktop::diagnostics::log(format!(
                    "download finished: {} ({} bytes)",
                    item.name(),
                    item.received
                ));
            },
            Err(error) => {
                let _ = std::fs::remove_file(&item.part_path);
                item.state = State::Failed;
                item.error = Some(format!("Could not save the file: {error}"));
            },
        }
        self.changed = true;
        self.save();
    }

    fn trim(&mut self) {
        while self.items.len() > MAX_ITEMS {
            match self
                .items
                .iter()
                .rposition(|item| item.state != State::Downloading)
            {
                Some(index) => {
                    self.items.remove(index);
                },
                None => break,
            }
        }
    }

    /// Commands from the Downloads panel (`dl-*`).
    pub(crate) fn command(
        &mut self,
        command: &str,
        argument: &str,
        servo: &Servo,
        webview: Option<WebViewId>,
    ) {
        let key: u64 = argument.parse().unwrap_or(0);
        match command {
            "dl-cancel" => {
                if let Some(handle) = self.handles.get(&key) {
                    handle.cancel();
                }
            },
            "dl-retry" => {
                let Some(item) = self.items.iter().find(|item| item.key == key) else {
                    return;
                };
                if item.state == State::Downloading {
                    return;
                }
                let Some(webview) = webview else {
                    return;
                };
                match Url::parse(&item.url) {
                    Ok(url) if !item.url.is_empty() => {
                        self.retries.push((item.url.clone(), key));
                        servo.network_manager().download(webview, url);
                    },
                    _ => {
                        if let Some(item) = self.item_mut(key) {
                            item.error = Some(
                                "This download can't be retried. Download it again from the page."
                                    .into(),
                            );
                        }
                    },
                }
            },
            "dl-open" => {
                if let Some(item) = self.items.iter().find(|item| item.key == key) &&
                    item.state == State::Done
                {
                    // Only on the user's explicit request (the panel asks first for programs).
                    open_file(&item.path);
                }
            },
            "dl-show" => {
                if let Some(item) = self.items.iter().find(|item| item.key == key) {
                    show_in_folder(&item.path);
                }
            },
            "dl-remove" => {
                if let Some(handle) = self.handles.remove(&key) {
                    handle.cancel();
                }
                self.items.retain(|item| item.key != key);
                self.save();
            },
            "dl-clear" => {
                self.items.retain(|item| item.state == State::Downloading);
                self.save();
            },
            "dl-ask" => {
                self.ask_where_to_save = argument == "1";
                self.save();
            },
            // The panel opened: check that finished files are still there.
            "dl-refresh" => {
                for item in &mut self.items {
                    item.missing = item.state == State::Done && !item.path.exists();
                }
            },
            "dl-open-folder" => open_file(&download_dir()),
            _ => warn!("unknown downloads command: {command}"),
        }
        self.changed = true;
    }

    /// The Downloads panel's state.
    pub(crate) fn to_json(&self) -> Value {
        let active: Vec<&Item> = self
            .items
            .iter()
            .filter(|item| item.state == State::Downloading)
            .collect();
        // Overall progress of the downloads in progress, if all their sizes are known.
        let progress =
            (!active.is_empty() && active.iter().all(|item| item.total.is_some())).then(|| {
                let total: u64 = active.iter().filter_map(|item| item.total).sum();
                let received: u64 = active.iter().map(|item| item.received).sum();
                if total == 0 {
                    0.0
                } else {
                    received as f64 / total as f64
                }
            });
        json!({
            "items": self.items.iter().map(Item::to_json).collect::<Vec<_>>(),
            "active": active.len(),
            "progress": progress,
            "askWhereToSave": self.ask_where_to_save,
            "folder": download_dir().display().to_string(),
        })
    }
}

/// The folder downloads are saved to. `STILLWATER_DOWNLOAD_DIR` overrides it (for tests).
fn download_dir() -> PathBuf {
    std::env::var_os("STILLWATER_DOWNLOAD_DIR")
        .map(PathBuf::from)
        .or_else(dirs::download_dir)
        .or_else(|| dirs::home_dir().map(|home| home.join("Downloads")))
        .unwrap_or_else(|| PathBuf::from("."))
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn part_path_for(path: &Path) -> PathBuf {
    let mut part = path.as_os_str().to_owned();
    part.push(".part");
    PathBuf::from(part)
}

fn is_executable(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            EXECUTABLE_EXTENSIONS
                .iter()
                .any(|executable| executable.eq_ignore_ascii_case(extension))
        })
}

/// A file name Windows accepts: no reserved characters or device names, no trailing dots
/// or spaces, not too long. Names without an extension get one from the MIME type.
fn sanitize_file_name(name: &str, mime_type: Option<&str>) -> String {
    let mut name: String = name
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    name = name.trim().trim_end_matches(['.', ' ']).to_owned();
    if name.trim_start_matches('.').is_empty() {
        name = "download".to_owned();
    }
    if Path::new(&name).extension().is_none() &&
        let Some(extension) = mime_type.and_then(extension_for_type)
    {
        name = format!("{name}.{extension}");
    }
    let stem = name
        .split('.')
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    const DEVICES: &[&str] = &["CON", "PRN", "AUX", "NUL", "CONIN$", "CONOUT$"];
    let is_numbered_device = (stem.starts_with("COM") || stem.starts_with("LPT")) &&
        stem.len() == 4 &&
        stem.as_bytes()[3].is_ascii_digit();
    if DEVICES.contains(&stem.as_str()) || is_numbered_device {
        name = format!("_{name}");
    }
    // Keep well under MAX_PATH, keeping the extension.
    const MAX_CHARS: usize = 150;
    if name.chars().count() > MAX_CHARS {
        let (stem, extension) = split_extension(&name);
        let keep = MAX_CHARS.saturating_sub(extension.chars().count());
        name = stem.chars().take(keep).collect::<String>() + extension;
    }
    name
}

/// The usual extension for common types, for names that have none (e.g. `data:` URLs).
fn extension_for_type(mime_type: &str) -> Option<&'static str> {
    Some(match mime_type {
        "text/plain" => "txt",
        "text/csv" => "csv",
        "text/html" => "html",
        "text/calendar" => "ics",
        "application/json" => "json",
        "application/xml" | "text/xml" => "xml",
        "application/pdf" => "pdf",
        "application/zip" => "zip",
        "application/gzip" => "gz",
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/svg+xml" => "svg",
        "audio/mpeg" => "mp3",
        "video/mp4" => "mp4",
        "video/webm" => "webm",
        _ => return None,
    })
}

/// "archive.tar.gz" -> ("archive", ".tar.gz"); "file.zip" -> ("file", ".zip").
fn split_extension(name: &str) -> (&str, &str) {
    let Some(dot) = name.rfind('.').filter(|dot| *dot > 0) else {
        return (name, "");
    };
    let (stem, extension) = name.split_at(dot);
    if let Some(tar) = stem
        .rfind(".tar")
        .filter(|tar| *tar > 0 && stem.len() - tar == 4)
    {
        return name.split_at(tar);
    }
    (stem, extension)
}

/// `directory/name`, or `directory/name (1).ext` and so on if that is taken.
fn unique_path(directory: &Path, name: &str, reserved: impl Fn(&Path) -> bool) -> PathBuf {
    let taken = |path: &Path| path.exists() || part_path_for(path).exists() || reserved(path);
    let path = directory.join(name);
    if !taken(&path) {
        return path;
    }
    let (stem, extension) = split_extension(name);
    (1..10_000)
        .map(|number| directory.join(format!("{stem} ({number}){extension}")))
        .find(|path| !taken(path))
        .unwrap_or(path)
}

/// Rename, or copy and delete when the destination is on another drive.
fn move_file(from: &Path, to: &Path) -> std::io::Result<()> {
    if std::fs::rename(from, to).is_ok() {
        return Ok(());
    }
    std::fs::copy(from, to)?;
    std::fs::remove_file(from)
}

/// Record where the file came from, like other browsers ("Mark of the Web"), so Windows warns
/// before running a downloaded program. The URL stays on this computer.
fn mark_as_downloaded(path: &Path, url: &str) {
    #[cfg(windows)]
    {
        let mut stream = path.as_os_str().to_owned();
        stream.push(":Zone.Identifier");
        let mut contents = String::from("[ZoneTransfer]\r\nZoneId=3\r\n");
        if url.starts_with("http://") || url.starts_with("https://") {
            contents.push_str(&format!("HostUrl={url}\r\n"));
        }
        if let Err(error) = std::fs::write(PathBuf::from(stream), contents) {
            warn!("Could not mark {} as downloaded: {error}", path.display());
        }
    }
    #[cfg(not(windows))]
    let _ = (path, url);
}

fn open_file(path: &Path) {
    #[cfg(windows)]
    let result = std::process::Command::new("explorer.exe").arg(path).spawn();
    #[cfg(not(windows))]
    let result = std::process::Command::new("xdg-open").arg(path).spawn();
    if let Err(error) = result {
        warn!("Could not open {}: {error}", path.display());
    }
}

fn show_in_folder(path: &Path) {
    #[cfg(windows)]
    let result = {
        use std::os::windows::process::CommandExt;
        if path.exists() {
            std::process::Command::new("explorer.exe")
                .raw_arg(format!("/select,\"{}\"", path.display()))
                .spawn()
        } else {
            std::process::Command::new("explorer.exe")
                .arg(path.parent().unwrap_or(path))
                .spawn()
        }
    };
    #[cfg(not(windows))]
    let result = std::process::Command::new("xdg-open")
        .arg(path.parent().unwrap_or(path))
        .spawn();
    if let Err(error) = result {
        warn!("Could not show {}: {error}", path.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_names() {
        assert_eq!(sanitize_file_name("a/b:c?.zip", None), "a_b_c_.zip");
        assert_eq!(sanitize_file_name("  report.pdf. ", None), "report.pdf");
        assert_eq!(sanitize_file_name("", None), "download");
        assert_eq!(sanitize_file_name("..", None), "download");
        assert_eq!(sanitize_file_name("con.txt", None), "_con.txt");
        assert_eq!(sanitize_file_name("COM1", None), "_COM1");
        assert_eq!(sanitize_file_name("export", Some("text/csv")), "export.csv");
        assert_eq!(split_extension("a.tar.gz"), ("a", ".tar.gz"));
        assert_eq!(split_extension("a.zip"), ("a", ".zip"));
        assert_eq!(split_extension(".hidden"), (".hidden", ""));
        assert_eq!(split_extension("noext"), ("noext", ""));
    }
}
