/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Incognito (private) windows.
//!
//! An incognito window is a separate Stillwater process, started with `--incognito`. Servo's
//! own "private" resource threads can't be used for this: no top-level page can be created
//! against them, and their localStorage, IndexedDB and Cache API storage share the normal
//! profile's folder. A process of its own isolates everything, whatever part of the engine
//! keeps it:
//!
//! - The engine's folder (`--config-dir`: cookies, localStorage, IndexedDB, Cache API, HSTS,
//!   HTTP auth) is a fresh temporary folder, deleted when the process ends. Another launch
//!   deletes any left behind by a crash (a live session holds a lock file in its folder).
//! - The HTTP cache is in memory only (`network_http_disk_cache` is empty).
//! - The shell writes nothing to the profile: no history, session, spaces, settings, downloads
//!   list, passwords, translation or ad-block settings (see `active()` where they are saved).
//!   Bookmarks the user adds are kept, and downloaded files stay in the Downloads folder.
//!   Saved passwords are still offered for filling, but nothing new is saved.
//! - Its diagnostic log is a separate file that only gets lines that can't hold a page's
//!   address or title (`loggable`).
//!
//! Further incognito requests (Ctrl+Shift+N, a link's "Open link in incognito window", a
//! launch with `--incognito`) join the running incognito process over its own single-instance
//! pipe and open as a new tab there, so all incognito tabs share one cookie jar. Closing the
//! last incognito window ends the process and deletes its folder.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use servo::{Opts, Preferences};
use url::Url;

use crate::desktop::diagnostics;
use crate::running_app_state::UserInterfaceCommand;
use crate::window::ServoShellWindow;

/// This process is an incognito session.
static ACTIVE: AtomicBool = AtomicBool::new(false);
/// This process hands URLs to (and takes them from) other launches.
static SINGLE_INSTANCE: AtomicBool = AtomicBool::new(false);

struct Session {
    dir: PathBuf,
    /// Held open (without delete sharing) while the session runs, so other launches don't
    /// delete the folder from under it.
    _lock: File,
}

static SESSION: Mutex<Option<Session>> = Mutex::new(None);
/// Arguments a process started from this one gets too (the engine folder to read preferences
/// from, headless): see `passthrough`.
static PASSTHROUGH: Mutex<Vec<String>> = Mutex::new(Vec::new());

const LOCK_FILE: &str = "session.lock";
const SESSION_PREFIX: &str = "session-";
const CLEANUP_ARGUMENT: &str = "--incognito-cleanup=";

/// Whether this process is an incognito session.
pub(crate) fn active() -> bool {
    ACTIVE.load(Ordering::Relaxed)
}

/// The window title's app name.
pub(crate) fn app_name() -> &'static str {
    if active() {
        "Stillwater Incognito"
    } else {
        "Stillwater"
    }
}

/// Where incognito sessions keep their temporary folders: the system's temporary folder, or
/// inside a scratch `STILLWATER_CONFIG_DIR` profile (tests).
fn base_dir() -> PathBuf {
    match std::env::var_os("STILLWATER_CONFIG_DIR").filter(|dir| !dir.is_empty()) {
        Some(dir) => PathBuf::from(dir).join("incognito-temp"),
        None => std::env::temp_dir().join("Stillwater-incognito"),
    }
}

/// Delete a session folder unless its session is still running. True if it is gone.
fn remove_if_unlocked(dir: &Path) -> bool {
    let lock = dir.join(LOCK_FILE);
    match std::fs::remove_file(&lock) {
        Ok(()) => {},
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            // A session creates its lock first thing: a folder without one that is only a few
            // seconds old may be a session starting right now.
            let young = std::fs::metadata(dir)
                .and_then(|metadata| metadata.modified())
                .ok()
                .and_then(|modified| modified.elapsed().ok())
                .is_none_or(|age| age < Duration::from_secs(30));
            if young {
                return false;
            }
        },
        // Held by the running session.
        Err(_) => return false,
    }
    std::fs::remove_dir_all(dir).is_ok() || !dir.exists()
}

/// Delete the folders of incognito sessions that ended without cleaning up (a crash, or files
/// still busy as the process ended). Runs at every launch.
fn clean_stale() {
    let base = base_dir();
    let Ok(entries) = std::fs::read_dir(&base) else {
        return;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        let is_session = entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.starts_with(SESSION_PREFIX));
        if is_session && path.is_dir() && remove_if_unlocked(&path) {
            removed += 1;
        }
    }
    if removed > 0 {
        diagnostics::log(format!(
            "incognito: deleted {removed} folder(s) left by an earlier incognito session"
        ));
    }
    let _ = std::fs::remove_dir(&base);
}

fn create_session() -> std::io::Result<Session> {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |time| time.as_nanos());
    let dir = base_dir().join(format!("{SESSION_PREFIX}{}-{stamp}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // FILE_SHARE_READ only: no one can delete it while it is open.
        options.share_mode(1);
    }
    let lock = options.open(dir.join(LOCK_FILE))?;
    std::fs::create_dir_all(dir.join("engine"))?;
    Ok(Session { dir, _lock: lock })
}

/// The arguments of this launch that a process started from it should get too.
fn passthrough(args: &[String]) -> Vec<String> {
    let mut kept = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--config-dir" || arg == "--pref" {
            if let Some(value) = iter.next() {
                kept.push(format!("{arg}={value}"));
            }
        } else if arg.starts_with("--config-dir=") ||
            arg.starts_with("--pref=") ||
            arg.starts_with("--window-size=") ||
            arg == "--headless" ||
            arg == "-z"
        {
            kept.push(arg.clone());
        }
    }
    kept
}

/// At launch, before the log opens: tidy up after earlier sessions and, for an incognito
/// launch, point the engine at a fresh temporary folder and keep the HTTP cache in memory.
/// An error means no private folder could be made: the launch must not go on.
pub(crate) fn startup(
    incognito: bool,
    single_instance: bool,
    args: &[String],
    opts: &mut Opts,
    preferences: &mut Preferences,
) -> std::io::Result<()> {
    SINGLE_INSTANCE.store(single_instance, Ordering::Relaxed);
    if let Ok(mut kept) = PASSTHROUGH.lock() {
        *kept = passthrough(args);
    }
    clean_stale();
    if !incognito {
        return Ok(());
    }
    let session = create_session()?;
    opts.config_dir = Some(session.dir.join("engine"));
    // Not `temporary_storage`: with it, Servo puts an HTTP disk cache in the temporary folder.
    opts.temporary_storage = false;
    preferences.network_http_disk_cache = String::new();
    if let Ok(mut slot) = SESSION.lock() {
        *slot = Some(session);
    }
    ACTIVE.store(true, Ordering::Relaxed);
    Ok(())
}

/// The session is over: delete its folder. Files the engine still holds open are deleted by a
/// helper process once this one has ended (or, failing that, by the next launch).
pub(crate) fn finish() {
    let Some(session) = SESSION.lock().ok().and_then(|mut slot| slot.take()) else {
        return;
    };
    let Session { dir, _lock } = session;
    drop(_lock);
    for _ in 0..10 {
        if std::fs::remove_dir_all(&dir).is_ok() || !dir.exists() {
            diagnostics::log_without_waiting("incognito: session folder deleted");
            let _ = std::fs::remove_dir(base_dir());
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let helper = std::env::current_exe().and_then(|exe| {
        let mut command = std::process::Command::new(exe);
        command.arg(format!("{CLEANUP_ARGUMENT}{}", dir.display()));
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // CREATE_NO_WINDOW
            command.creation_flags(0x0800_0000);
        }
        command.spawn()
    });
    match helper {
        Ok(child) => diagnostics::log_without_waiting(format!(
            "incognito: some files were busy; cleanup process {} deletes them",
            child.id()
        )),
        Err(_) => diagnostics::log_without_waiting(
            "incognito: some files were busy; the next launch deletes them",
        ),
    }
}

/// `--incognito-cleanup=<folder>` (started by `finish`): wait for the session's files to be
/// released and delete its folder. Returns true if this launch was such a helper.
pub(crate) fn run_cleanup_helper(args: &[String]) -> bool {
    let Some(dir) = args.first().and_then(|arg| arg.strip_prefix(CLEANUP_ARGUMENT)) else {
        return false;
    };
    let dir = PathBuf::from(dir);
    // Only ever a session folder in the incognito folder.
    let is_session = dir.parent() == Some(base_dir().as_path()) &&
        dir.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with(SESSION_PREFIX));
    if is_session {
        for _ in 0..120 {
            if std::fs::remove_dir_all(&dir).is_ok() || !dir.exists() {
                let _ = std::fs::remove_dir(base_dir());
                break;
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }
    true
}

/// The log file for an incognito session (next to the normal one, which it must not replace).
pub(crate) fn log_file_name() -> &'static str {
    "stillwater-incognito.log"
}

/// Whether an incognito session may log `message`: only lines that can't hold a page's
/// address, title or anything typed. (Test runs may also log what their scripts report.)
pub(crate) fn loggable(message: &str) -> bool {
    const SAFE: &[&str] = &[
        "incognito:",
        "shutdown",
        "summary:",
        "timer resolution:",
        "memory",
        "resumed:",
        "window focused=",
        "sidebar page ready",
        "single instance: listening",
        "single instance: could not",
        "single instance: a new launch arrived while quitting",
        "single instance: the event loop didn't take",
        "event loop",
    ];
    if SAFE.iter().any(|prefix| message.starts_with(prefix)) {
        return true;
    }
    std::env::var_os("STILLWATER_TEST_SCRIPT").is_some() &&
        (message.starts_with("sidebar debug: ") || message.starts_with("page eval: "))
}

/// Start a Stillwater process: `--incognito` and `url`, with this launch's pass-through
/// arguments. Returns the process id.
fn spawn(incognito: bool, url: &str) -> std::io::Result<u32> {
    let mut command = std::process::Command::new(std::env::current_exe()?);
    if incognito {
        command.arg("--incognito");
    }
    if let Ok(kept) = PASSTHROUGH.lock() {
        command.args(kept.iter());
    }
    command.arg(url);
    command.spawn().map(|child| child.id())
}

/// Open `url` (or a new tab) in an incognito window: in the running incognito process, or in a
/// new one. Doesn't wait (handing over can take a moment).
fn launch_incognito(url: String) {
    let _ = std::thread::Builder::new()
        .name("IncognitoLaunch".into())
        .spawn(move || {
            if SINGLE_INSTANCE.load(Ordering::Relaxed) &&
                crate::desktop::single_instance::forward_to_incognito_instance(&url)
            {
                diagnostics::log("incognito: opened in the running incognito window");
                return;
            }
            match spawn(true, &url) {
                Ok(pid) => diagnostics::log(format!("incognito: started process {pid}")),
                Err(error) => diagnostics::log(format!("incognito: could not start: {error}")),
            }
        });
}

/// From an incognito window: open `url` (a Stillwater page) in a normal window instead.
fn open_in_normal_window(url: String) {
    let _ = std::thread::Builder::new()
        .name("NormalLaunch".into())
        .spawn(move || {
            if SINGLE_INSTANCE.load(Ordering::Relaxed) &&
                crate::desktop::single_instance::forward_to_running_instance(&url)
            {
                diagnostics::log("incognito: handed a Stillwater page to a normal window");
                return;
            }
            match spawn(false, &url) {
                Ok(pid) => diagnostics::log(format!(
                    "incognito: started normal process {pid} for a Stillwater page"
                )),
                Err(error) => {
                    diagnostics::log(format!("incognito: could not start a normal window: {error}"))
                },
            }
        });
}

/// "New incognito window" (Ctrl+Shift+N, menus) and "Open link in incognito window". In an
/// incognito window, a new tab there.
pub(crate) fn open(window: &ServoShellWindow, url: Option<Url>) {
    let url = url.filter(|url| matches!(url.scheme(), "http" | "https"));
    if active() {
        let url = url.unwrap_or_else(|| Url::parse("servo:newtab").expect("valid URL"));
        window.queue_user_interface_command(UserInterfaceCommand::NewWebViewWithUrl(url));
        return;
    }
    launch_incognito(url.map_or_else(|| "servo:newtab".to_owned(), String::from));
}

/// Sidebar commands that incognito handles or changes. True if `command` was handled here.
pub(crate) fn sidebar_command(window: &ServoShellWindow, command: &str, argument: &str) -> bool {
    if command == "incognito-window" {
        open(window, Url::parse(argument).ok());
        return true;
    }
    if !active() {
        return false;
    }
    match command {
        // Settings, saved passwords and importing change the profile: they open in a normal
        // window.
        "open-settings" => {
            let section: String = argument
                .chars()
                .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
                .collect();
            let url = if section.is_empty() {
                crate::desktop::sidebar::SETTINGS_URL.to_owned()
            } else {
                format!("{}#{section}", crate::desktop::sidebar::SETTINGS_URL)
            };
            open_in_normal_window(url);
            true
        },
        // Nothing about incognito pages leaves the window: no reports (they open the page's
        // address in another browser), no comparisons, no imports, nothing saved.
        "report-site" | "compare" | "zen-scan" | "zen-import" | "pw-save" | "pw-never" |
        "pw-delete" | "pw-autofill" | "pw-forget-never" => true,
        _ => false,
    }
}
