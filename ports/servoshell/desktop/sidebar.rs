/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! A vertical-tab sidebar whose UI is an HTML page rendered by Servo itself
//! (`servo:sidebar/index.html`), styled after Zen Browser.
//!
//! Page -> Rust: `console.log("__sidebar__<command>\t<argument>")`.
//! Rust -> Page: `window.sidebar.setState(<json>)` via `evaluate_javascript`.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;
use std::task::Waker;

use dpi::PhysicalSize;
use euclid::{Point2D, Scale};
use log::warn;
use servo::{
    ConsoleLogLevel, Cursor, DeviceIndependentPixel, DevicePixel, LoadStatus, NavigationRequest,
    OffscreenRenderingContext, RenderingContext, Theme, WebView, WebViewBuilder, WebViewDelegate,
    WebViewId,
};
use url::Url;

use crate::desktop::event_loop::RepaintEventLoopWaker;
use crate::desktop::find_bar::FindCommand;
use crate::desktop::settings::{Settings, ThemeChoice};
use crate::desktop::updater::{UpdateInfo, UpdateStatus};
use crate::desktop::workspaces::{TabEntry, TabTarget, Workspaces};
use crate::desktop::panes::PaneKind;
use crate::desktop::passwords::PasswordAction;
use crate::desktop::workspaces::ImportCounts;
use crate::desktop::zen_import::ZenSession;
use crate::running_app_state::{RunningAppState, UserInterfaceCommand};
use crate::window::ServoShellWindow;

/// Session restore, reopening closed tabs, drag and drop.
mod session;

/// Default width of the expanded sidebar, in device-independent pixels. The user can drag
/// the sidebar's right edge between `MIN_SIDEBAR_WIDTH` and `MAX_SIDEBAR_FRACTION` of the window.
pub(crate) const SIDEBAR_WIDTH: f32 = 240.0;
pub(crate) const MIN_SIDEBAR_WIDTH: f32 = 180.0;
pub(crate) const MAX_SIDEBAR_FRACTION: f32 = 0.4;
/// Gap between the window edge / sidebar and the rounded web content card.
pub(crate) const CONTENT_MARGIN: f32 = 8.0;
/// Corner radius of the web content card.
pub(crate) const CONTENT_RADIUS: f32 = 8.0;
/// In compact mode, hovering within this many pixels of the left edge reveals the sidebar.
const PEEK_EDGE: f32 = 10.0;

const MESSAGE_PREFIX: &str = "__sidebar__";

/// The settings page, opened in a tab (`servo:settings`, with `#<category>`).
pub(crate) const SETTINGS_URL: &str = "servo:settings";
/// Console messages from the settings page start with this.
pub(crate) const SETTINGS_MESSAGE_PREFIX: &str = "__stillwater_settings__";
/// The commands the settings page may send: settings only, nothing that acts on other tabs.
const SETTINGS_COMMANDS: &[&str] = &[
    "ready", "set-theme", "set-search", "set-tab-unload", "set-unload-low-memory", "set-newtab",
    "toggle-compact", "session-startup", "adblock-enabled", "pw-autofill", "pw-delete", "pw-reveal",
    "pw-copy", "pw-copy-user", "pw-forget-never", "zen-scan", "zen-import", "check-updates",
    "hist-clear", "hist-show", "dl-ask", "dl-panel", "set-spaces", "tr-auto", "tr-target", "tr-unlist",
    "tr-delete-model", "tr-models", "set-splash", "set-scrollbar",
];

/// Whether `url` is the settings page (whatever category it shows).
pub(crate) fn is_settings_url(url: &Url) -> bool {
    url.scheme() == "servo" && url.path() == "settings"
}
const SIDEBAR_URL: &str = "servo:sidebar/index.html";

/// A tab stops showing as loading after this long even without a load event (e.g. a
/// navigation that answered 204 No Content and so never completes).
const LOADING_LIMIT: std::time::Duration = std::time::Duration::from_secs(60);

thread_local! {
    /// Tabs with a page load in flight, from the moment it was asked for: Servo reports
    /// `LoadStatus::Started` only once the response arrives (with `Complete` right behind on
    /// fast pages), so it can't drive the tab's loading indicator by itself.
    static NAVIGATING: RefCell<HashMap<WebViewId, std::time::Instant>> = RefCell::new(HashMap::new());
}

/// A page load started or was asked for in `webview` (a link, the address bar, reload).
pub(crate) fn navigation_started(webview: WebViewId) {
    NAVIGATING.with(|map| map.borrow_mut().insert(webview, std::time::Instant::now()));
}

/// Whether going to `target` only jumps to a `#fragment` of the page showing (`current`),
/// which loads nothing and so never completes.
pub(crate) fn is_same_document(current: Option<Url>, target: &Url) -> bool {
    target.fragment().is_some() &&
        current.is_some_and(|mut current| {
            let mut target = target.clone();
            current.set_fragment(None);
            target.set_fragment(None);
            current == target
        })
}

/// `webview`'s load finished, failed, became a download, or the tab closed.
pub(crate) fn navigation_finished(webview: WebViewId) {
    NAVIGATING.with(|map| map.borrow_mut().remove(&webview));
}

/// Whether `webview` is loading a page, for the tab's loading indicator.
fn is_navigating(webview: &WebView) -> bool {
    let pending = NAVIGATING.with(|map| {
        map.borrow()
            .get(&webview.id())
            .is_some_and(|started| started.elapsed() < LOADING_LIMIT)
    });
    pending || matches!(webview.load_status(), LoadStatus::Started | LoadStatus::HeadParsed)
}

/// Window operations requested by the sidebar's own caption buttons.
#[derive(Clone, Debug)]
pub(crate) enum WindowAction {
    Minimize,
    ToggleMaximize,
    Close,
    Drag,
    /// Install the available update (or open its release page for portable copies).
    Update,
    /// Check GitHub for an update right now (settings panel).
    CheckForUpdates,
    /// Report a tab (or the active one) as a broken site.
    ReportSite(Option<WebViewId>),
    /// Show a page in split view beside the active tab, or in Glance over it.
    OpenPane(PaneKind, url::Url),
    /// Close the split view or Glance pane.
    ClosePane(PaneKind),
    /// Turn ad blocking off (or back on) for the current page's site, and reload it.
    AdblockToggleSite,
    /// Turn ad blocking on or off everywhere.
    AdblockSetEnabled(bool),
    /// Save, fill, copy, show or delete saved passwords.
    Password(PasswordAction),
    /// Compare the page in the URL bar with the same page in Microsoft Edge.
    Compare,
    /// Look for Zen Browser profiles to import from, and count what profile number N holds.
    ZenScan(usize),
    /// Import from a Zen profile (index from the scan): spaces and pins, open tabs, passwords,
    /// bookmarks, history, cookies.
    ZenImport {
        profile: usize,
        pins: bool,
        tabs: bool,
        passwords: bool,
        history: bool,
        bookmarks: bool,
        cookies: bool,
    },
    /// History, address-bar suggestions and bookmarks: a `hist-` / `bm-` command and its argument.
    History(String, String),
    /// New tab: open (or close) Zen's floating address bar over the page.
    CommandBar,
    /// A command from the Downloads panel (`dl-*`) and its argument.
    Download(String, String),
    /// A page translation command (`tr-*`) and its argument.
    Translate(String, String),
    /// Drive the find bar (`find-*` commands).
    Find(FindCommand),
}

struct SidebarDelegate {
    inbox: Rc<RefCell<Vec<String>>>,
    cursor: Rc<Cell<Cursor>>,
    waker: Waker,
}

impl WebViewDelegate for SidebarDelegate {
    fn show_console_message(&self, _: WebView, _: ConsoleLogLevel, message: String) {
        match message.strip_prefix(MESSAGE_PREFIX) {
            Some(message) => {
                self.inbox.borrow_mut().push(message.to_owned());
                self.waker.wake_by_ref();
            },
            None => println!("[sidebar] {message}"),
        }
    }

    fn notify_new_frame_ready(&self, _: WebView) {
        self.waker.wake_by_ref();
    }

    fn notify_cursor_changed(&self, _: WebView, cursor: Cursor) {
        self.cursor.set(cursor);
    }

    fn request_navigation(&self, _: WebView, request: NavigationRequest) {
        // The sidebar must never navigate away from its own page.
        if request.url.as_str().starts_with("servo:sidebar/") {
            request.allow();
        } else {
            request.deny();
        }
    }
}

pub(crate) struct Sidebar {
    rendering_context: Rc<OffscreenRenderingContext>,
    webview: Option<WebView>,
    inbox: Rc<RefCell<Vec<String>>>,
    cursor: Rc<Cell<Cursor>>,
    waker: Waker,
    /// The last state JSON pushed to the page, to avoid redundant script evaluations.
    last_pushed_state: Option<String>,
    /// Small stable numeric ids handed to the page for each tab.
    tab_ids: HashMap<WebViewId, u32>,
    next_tab_id: u32,
    /// PNG data URLs of tab favicons.
    favicons: HashMap<WebViewId, String>,
    compact: Cell<bool>,
    peek: Cell<bool>,
    /// The width the user chose (unclamped to the current window; see `width_for`).
    width: Cell<f32>,
    focused: Cell<bool>,
    workspaces: Workspaces,
    theme: Theme,
    window_actions: Vec<WindowAction>,
    frameless: bool,
    maximized: bool,
    update: Option<UpdateInfo>,
    update_status: Option<UpdateStatus>,
    /// The sidebar page has loaded and received its first state.
    ready: bool,
    settings: RefCell<Settings>,
    /// Result of a manual update check, shown in the settings panel.
    update_check: Option<String>,
    /// The split / Glance pane with keyboard focus: the URL bar and navigation act on it.
    pane_focus: Option<WebView>,
    /// (title, url) of the open split and Glance panes.
    panes: (Option<(String, String)>, Option<(String, String)>),
    /// Ad blocking state for the shield and settings.
    adblock: serde_json::Value,
    /// Page translation state for the translate button and settings.
    translate: serde_json::Value,
    /// Tabs playing audio or video, for the speaker icon.
    playing_media: HashSet<WebViewId>,
    /// Saved passwords (without the passwords) and the save offer.
    passwords: serde_json::Value,
    /// Import from Zen: profiles found, what they hold, the last result.
    zen: serde_json::Value,
    /// Compare with Edge: whether it's running and what to tell the user.
    compare: serde_json::Value,
    /// Open settings pages and the state last sent to each.
    settings_pages: Vec<(WebView, Option<String>)>,
    /// The settings page that asked to see a password, if one did.
    reveal_target: Option<WebView>,
    /// Saves the open tabs (only the first window's sidebar has one).
    session: Option<crate::desktop::session::SessionWriter>,
    /// History and bookmarks: whether the page in the URL bar is bookmarked.
    hist: serde_json::Value,
    /// The floating new-tab address bar is open: no tab shows as selected.
    command_bar: bool,
    /// Downloads in progress and finished, for the downloads indicator and panel.
    downloads: serde_json::Value,
}

impl Sidebar {
    pub(crate) fn new(
        rendering_context: Rc<OffscreenRenderingContext>,
        waker: RepaintEventLoopWaker,
    ) -> Self {
        let settings = Settings::load();
        let (workspaces, next_tab_id) = session::load_workspaces();
        Self {
            rendering_context,
            webview: None,
            inbox: Default::default(),
            cursor: Default::default(),
            waker: Waker::from(Arc::new(waker)),
            last_pushed_state: None,
            tab_ids: Default::default(),
            next_tab_id,
            favicons: Default::default(),
            // STILLWATER_START_COMPACT: start with the sidebar collapsed (used for testing).
            compact: Cell::new(
                settings.compact || std::env::var_os("STILLWATER_START_COMPACT").is_some(),
            ),
            peek: Cell::new(false),
            width: Cell::new(settings.sidebar_width),
            focused: Cell::new(false),
            workspaces,
            theme: Theme::Light,
            window_actions: Vec::new(),
            frameless: false,
            maximized: false,
            update: None,
            update_status: None,
            ready: false,
            pane_focus: None,
            panes: (None, None),
            adblock: serde_json::Value::Null,
            translate: serde_json::Value::Null,
            playing_media: HashSet::new(),
            passwords: serde_json::Value::Null,
            zen: serde_json::Value::Null,
            compare: serde_json::Value::Null,
            settings_pages: Vec::new(),
            reveal_target: None,
            hist: serde_json::Value::Null,
            command_bar: false,
            downloads: serde_json::Value::Null,
            settings: RefCell::new(settings),
            update_check: None,
            session: crate::desktop::session::SessionWriter::claim(),
        }
    }

    /// The theme chosen in settings, or None to follow Windows.
    pub(crate) fn theme_override(&self) -> Option<Theme> {
        self.settings.borrow().theme.override_theme()
    }

    /// Show the result of a manual update check in the settings panel.
    pub(crate) fn set_update_check(&mut self, message: String) {
        self.update_check = Some(message);
        self.last_pushed_state = None;
    }

    /// Window operations requested since the last call (caption buttons, dragging).
    pub(crate) fn take_window_actions(&mut self) -> Vec<WindowAction> {
        std::mem::take(&mut self.window_actions)
    }

    /// Whether the sidebar page has loaded (used to show the window without a blank frame).
    pub(crate) fn is_ready(&self) -> bool {
        self.ready
    }

    pub(crate) fn set_update_available(&mut self, update: UpdateInfo) {
        self.update = Some(update);
        self.last_pushed_state = None;
    }

    pub(crate) fn set_update_status(&mut self, status: UpdateStatus) {
        self.update_status = Some(status);
        self.last_pushed_state = None;
    }

    pub(crate) fn update(&self) -> Option<&UpdateInfo> {
        self.update.as_ref()
    }

    /// Whether an update is already downloading or installing.
    pub(crate) fn update_in_progress(&self) -> bool {
        matches!(
            self.update_status,
            Some(UpdateStatus::Downloading | UpdateStatus::Installing)
        )
    }

    /// Tell the page whether to draw its own caption buttons and which maximise icon to use.
    pub(crate) fn set_window_state(&mut self, frameless: bool, maximized: bool) {
        if (self.frameless, self.maximized) != (frameless, maximized) {
            self.frameless = frameless;
            self.maximized = maximized;
            self.last_pushed_state = None;
        }
    }

    /// Tell the sidebar which pane has focus and what the split / Glance panes show.
    pub(crate) fn set_panes(
        &mut self,
        focus: Option<WebView>,
        split: Option<(String, String)>,
        glance: Option<(String, String)>,
    ) {
        self.pane_focus = focus;
        self.panes = (split, glance);
    }

    pub(crate) fn set_playing_media(&mut self, playing_media: HashSet<WebViewId>) {
        self.playing_media = playing_media;
    }

    pub(crate) fn set_translate(&mut self, translate: serde_json::Value) {
        self.translate = translate;
    }

    pub(crate) fn set_adblock(&mut self, adblock: serde_json::Value) {
        self.adblock = adblock;
    }

    pub(crate) fn set_passwords(&mut self, passwords: serde_json::Value) {
        // A new offer to save a password: show the sidebar if compact mode hid it.
        if !passwords["offer"].is_null() && passwords["offer"] != self.passwords["offer"] && self.compact.get() {
            self.peek.set(true);
        }
        self.passwords = passwords;
    }

    pub(crate) fn set_hist(&mut self, hist: serde_json::Value) {
        self.hist = hist;
    }

    pub(crate) fn set_command_bar(&mut self, open: bool) {
        self.command_bar = open;
    }

    /// Whether New tab opens the floating address bar (settings).
    pub(crate) fn new_tab_bar(&self) -> bool {
        self.settings.borrow().new_tab_bar
    }

    /// Every tab and pin in every space except the active tab, with the sidebar command and
    /// argument that shows it, for the address bar's "Switch to tab" suggestions.
    pub(crate) fn switch_targets(&mut self, window: &ServoShellWindow) -> Vec<serde_json::Value> {
        self.sync_workspaces(window);
        let targets = self.workspaces.switch_targets(window);
        targets
            .into_iter()
            .map(|(target, title, url, space)| {
                let (command, argument, pinned) = match target {
                    TabTarget::Tab(TabEntry::Live(id)) => ("activate", self.tab_key(id), false),
                    TabTarget::Tab(TabEntry::Unloaded(key)) => ("activate", key, false),
                    TabTarget::Pin(pin) => ("pin-open", pin, true),
                };
                serde_json::json!({
                    "title": title, "url": url, "space": space,
                    "command": command, "argument": argument, "pinned": pinned,
                })
            })
            .collect()
    }

    pub(crate) fn set_downloads(&mut self, downloads: serde_json::Value) {
        // A new download: show the sidebar if compact mode hid it, so its progress is visible.
        if downloads["active"].as_u64() > self.downloads["active"].as_u64() && self.compact.get() {
            self.peek.set(true);
        }
        self.downloads = downloads;
    }

    pub(crate) fn set_compare(&mut self, compare: serde_json::Value) {
        self.compare = compare;
    }

    /// Whether Compare with Edge is already running.
    pub(crate) fn comparing(&self) -> bool {
        self.compare["busy"].as_bool() == Some(true)
    }

    pub(crate) fn set_zen(&mut self, zen: serde_json::Value) {
        self.zen = zen;
        // Always send it, even if nothing changed: the page shows "Looking for Zen…" until then.
        self.last_pushed_state = None;
    }

    /// Show a saved password the user asked to see. It's sent once, not kept in the state.
    pub(crate) fn reveal_password(&mut self, id: u64, password: &str) {
        let password = serde_json::to_string(password).unwrap_or_default();
        // Back to the settings page that asked, if it still shows the settings page.
        match self.reveal_target.take() {
            Some(page) if page.url().is_some_and(|url| is_settings_url(&url)) => {
                page.evaluate_javascript(
                    format!("location.protocol === 'servo:' && location.pathname === 'settings' && window.top === window && window.stillwaterSettings.revealPassword({id}, {password})"),
                    |_| {},
                );
            },
            _ => self.call(&format!("revealPassword({id}, {password})")),
        }
    }

    /// A message from a settings page (`SETTINGS_MESSAGE_PREFIX` removed). The caller has checked
    /// that `page` shows `servo:settings`; only settings commands are accepted.
    pub(crate) fn post_from_settings(&mut self, page: &WebView, message: &str) {
        let command = message.split_once('\t').map_or(message, |(command, _)| command);
        if !SETTINGS_COMMANDS.contains(&command) {
            warn!("ignored settings page message: {command}");
            return;
        }
        match command {
            // A (re)loaded page: send it the whole state.
            "ready" => {
                self.settings_pages.retain(|(other, _)| other.id() != page.id());
                self.settings_pages.push((page.clone(), None));
            },
            "pw-reveal" => {
                self.reveal_target = Some(page.clone());
                self.post(message);
            },
            _ => self.post(message),
        }
        self.waker.wake_by_ref();
    }

    /// Run `script` (a call on `window.stillwaterSettings`) in every open settings page.
    pub(crate) fn call_settings(&self, script: &str) {
        for (page, _) in &self.settings_pages {
            if page.url().is_some_and(|url| is_settings_url(&url)) {
                page.evaluate_javascript(
                    format!("location.protocol === 'servo:' && location.pathname === 'settings' && window.top === window && window.stillwaterSettings.{script}"),
                    |_| {},
                );
            }
        }
    }

    /// Show the settings page (`section` = a category), in its tab if one is open.
    fn open_settings(&mut self, window: &ServoShellWindow, section: &str) {
        let section: String = section.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').collect();
        let target = if section.is_empty() {
            SETTINGS_URL.to_owned()
        } else {
            format!("{SETTINGS_URL}#{section}")
        };
        let Ok(url) = Url::parse(&target) else {
            return;
        };
        let open = window
            .webviews()
            .into_iter()
            .find(|(_, webview)| webview.url().is_some_and(|url| is_settings_url(&url)));
        match open {
            Some((id, webview)) => {
                window.activate_webview(id);
                if !section.is_empty() {
                    webview.load(url);
                }
            },
            None => window.queue_user_interface_command(UserInterfaceCommand::NewWebViewWithUrl(url)),
        }
    }

    /// Add Zen's spaces and tabs to the sidebar.
    pub(crate) fn import_zen_session(&mut self, session: &ZenSession, pins: bool, tabs: bool) -> ImportCounts {
        let next_tab_id = &mut self.next_tab_id;
        let counts = self.workspaces.import_zen(session, pins, tabs, || {
            *next_tab_id += 1;
            *next_tab_id
        });
        self.last_pushed_state = None;
        counts
    }

    /// The page the URL bar and navigation buttons act on: the focused pane or the active tab.
    pub(crate) fn navigated_webview(&self, window: &ServoShellWindow) -> Option<WebView> {
        self.pane_focus.clone().or_else(|| window.active_webview())
    }

    /// Queue a command as if the sidebar page had sent it (used by keyboard shortcuts).
    pub(crate) fn post(&self, message: &str) {
        self.inbox.borrow_mut().push(message.to_owned());
        self.waker.wake_by_ref();
    }

    /// Whether the startup animation is turned on in settings.
    pub(crate) fn startup_animation(&self) -> bool {
        self.settings.borrow().startup_animation
    }

    /// The window background colour, tinted by the current workspace.
    pub(crate) fn background(&self) -> [u8; 3] {
        self.workspaces.background(self.theme)
    }

    pub(crate) fn webview(&self) -> Option<&WebView> {
        self.webview.as_ref()
    }

    pub(crate) fn cursor(&self) -> Cursor {
        self.cursor.get()
    }

    /// Create the sidebar `WebView` on first use (Servo isn't available when the window is built).
    pub(crate) fn ensure_webview(
        &mut self,
        state: &RunningAppState,
        theme: Theme,
        scale: Scale<f32, DeviceIndependentPixel, DevicePixel>,
    ) {
        if let Some(webview) = &self.webview {
            if self.theme != theme {
                self.theme = theme;
                webview.notify_theme_change(theme);
                self.last_pushed_state = None;
            }
            return;
        }
        self.theme = theme;
        let delegate = Rc::new(SidebarDelegate {
            inbox: self.inbox.clone(),
            cursor: self.cursor.clone(),
            waker: self.waker.clone(),
        });
        let webview = WebViewBuilder::new(state.servo(), self.rendering_context.clone())
            .url(Url::parse(SIDEBAR_URL).expect("valid sidebar URL"))
            .hidpi_scale_factor(scale)
            .delegate(delegate)
            .build();
        webview.notify_theme_change(theme);
        webview.show();
        self.webview = Some(webview);
    }

    /// Lay out the window: returns the sidebar rect (if visible) and the web content rect,
    /// both in device-independent pixels.
    pub(crate) fn layout(&self, full: egui::Rect, fullscreen: bool) -> (Option<egui::Rect>, egui::Rect) {
        if fullscreen {
            return (None, full);
        }
        let m = CONTENT_MARGIN;
        let width = self.width_for(full.width());
        if self.compact.get() {
            let content = full.shrink(m);
            let sidebar = self.peek.get().then(|| {
                egui::Rect::from_min_max(
                    egui::pos2(full.min.x + m, full.min.y + m),
                    egui::pos2(full.min.x + m + width, full.max.y - m),
                )
            });
            (sidebar, content)
        } else {
            let sidebar = egui::Rect::from_min_max(
                full.min,
                egui::pos2(full.min.x + width, full.max.y),
            );
            let content = egui::Rect::from_min_max(
                egui::pos2(sidebar.max.x, full.min.y + m),
                egui::pos2(full.max.x - m, full.max.y - m),
            );
            (Some(sidebar), content)
        }
    }

    /// Resize and paint the sidebar `WebView` into its own offscreen buffer.
    pub(crate) fn paint(&self, rect: egui::Rect, scale: Scale<f32, DeviceIndependentPixel, DevicePixel>) {
        let Some(webview) = &self.webview else {
            return;
        };
        if webview.hidpi_scale_factor() != scale {
            webview.set_hidpi_scale_factor(scale);
        }
        let size = PhysicalSize::new(
            (rect.width() * scale.get()) as u32,
            (rect.height() * scale.get()) as u32,
        );
        if webview.size().to_u32() != euclid::Size2D::new(size.width, size.height) {
            webview.resize(size);
        }
        self.rendering_context
            .make_current()
            .expect("Could not make sidebar RenderingContext current");
        webview.paint();
        self.rendering_context.present();
    }

    pub(crate) fn rendering_context(&self) -> &Rc<OffscreenRenderingContext> {
        &self.rendering_context
    }

    /// In compact mode, show the sidebar when the mouse touches the left edge and hide it
    /// again once the mouse leaves. Returns true if visibility changed.
    pub(crate) fn update_peek(&self, point: Point2D<f32, DeviceIndependentPixel>) -> bool {
        if !self.compact.get() {
            return false;
        }
        let peek = if self.peek.get() {
            point.x <= CONTENT_MARGIN + self.width.get() + 12.0
        } else {
            point.x <= PEEK_EDGE
        };
        self.peek.replace(peek) != peek
    }

    /// The sidebar width to use in a window `window_width` wide.
    pub(crate) fn width_for(&self, window_width: f32) -> f32 {
        let max = (window_width * MAX_SIDEBAR_FRACTION).max(MIN_SIDEBAR_WIDTH);
        self.width.get().clamp(MIN_SIDEBAR_WIDTH, max)
    }

    /// Whether the sidebar is docked (not compact), so its right edge can be dragged.
    pub(crate) fn is_resizable(&self) -> bool {
        !self.compact.get()
    }

    /// Live resize while dragging the edge. Returns true if the width changed.
    pub(crate) fn drag_width(&self, width: f32, window_width: f32) -> bool {
        let max = (window_width * MAX_SIDEBAR_FRACTION).max(MIN_SIDEBAR_WIDTH);
        let width = width.clamp(MIN_SIDEBAR_WIDTH, max).round();
        self.width.replace(width) != width
    }

    /// Save the width once the drag ends (not on every mouse move).
    pub(crate) fn commit_width(&self) {
        self.settings.borrow_mut().set_sidebar_width(self.width.get());
    }

    /// Double-click on the edge: back to the default width.
    pub(crate) fn reset_width(&self) {
        self.width.set(SIDEBAR_WIDTH);
        self.commit_width();
    }

    pub(crate) fn is_focused(&self) -> bool {
        self.focused.get()
    }

    /// Move keyboard focus between the sidebar and the active web content.
    pub(crate) fn set_focused(&self, focused: bool, window: &ServoShellWindow) {
        // Unfocusing the sidebar's WebView doesn't blur the element focused inside it, so the
        // address bar would keep its edited text and stop following the page. Done even if
        // Rust thought the sidebar unfocused already: the page can focus its field by itself.
        if !focused {
            self.call("leaveUrl()");
        }
        if self.focused.replace(focused) == focused {
            return;
        }
        // Neither side counts as focused while Stillwater isn't the active window.
        let window_focused = window.has_system_focus();
        if let Some(webview) = &self.webview {
            webview.set_focused(focused && window_focused);
        }
        if let Some(active) = window.active_webview() {
            active.set_focused(!focused && window_focused);
        }
    }

    pub(crate) fn call(&self, script: &str) {
        if let Some(webview) = &self.webview {
            webview.evaluate_javascript(
                format!("window.sidebar && window.sidebar.{script}"),
                |result| {
                    if let Err(error) = result {
                        warn!("sidebar script failed: {error:?}");
                    }
                },
            );
        }
    }

    pub(crate) fn focus_url(&self, window: &ServoShellWindow) {
        if self.compact.get() {
            self.peek.set(true);
        }
        self.set_focused(true, window);
        self.call("focusUrl()");
    }

    pub(crate) fn toggle_compact(&self) {
        self.compact.set(!self.compact.get());
        self.peek.set(false);
        self.settings.borrow_mut().set_compact(self.compact.get());
    }

    pub(crate) fn set_favicon(&mut self, id: WebViewId, data_url: String) {
        self.favicons.insert(id, data_url);
        self.last_pushed_state = None;
    }

    fn webview_for_tab(&self, window: &ServoShellWindow, tab: &str) -> Option<WebViewId> {
        let tab: u32 = tab.parse().ok()?;
        let (id, _) = self.tab_ids.iter().find(|(_, value)| **value == tab)?;
        window.webview_by_id(*id).map(|webview| webview.id())
    }

    /// The sidebar id of an unloaded tab, if `tab` names one.
    fn unloaded_tab(&self, tab: &str) -> Option<u32> {
        tab.parse()
            .ok()
            .filter(|key| self.workspaces.is_unloaded(*key))
    }

    /// Bring the workspace model up to date with the window's tabs. Tabs that were just loaded
    /// again after being unloaded keep their sidebar row (id) and icon.
    fn sync_workspaces(&mut self, window: &ServoShellWindow) {
        self.workspaces.sync(window);
        for (webview, key, favicon) in self.workspaces.take_restored() {
            self.tab_ids.insert(webview, key);
            if let Some(favicon) = favicon {
                self.favicons.entry(webview).or_insert(favicon);
            }
        }
    }

    /// The sidebar id of an open tab (allocating one if the page hasn't seen it yet).
    fn tab_key(&mut self, webview: WebViewId) -> u32 {
        *self.tab_ids.entry(webview).or_insert_with(|| {
            self.next_tab_id += 1;
            self.next_tab_id
        })
    }

    fn unload_tab(&mut self, window: &ServoShellWindow, webview: WebViewId) -> bool {
        let key = self.tab_key(webview);
        let favicon = self.favicons.get(&webview).cloned();
        self.workspaces.unload_tab(window, webview, key, favicon)
    }

    /// Unload background tabs that haven't been used for the time chosen in settings and,
    /// if `low_memory`, the least recently used one regardless. Tabs in `keep` (playing audio
    /// or video) and the active tab are never unloaded. Returns how many were unloaded.
    pub(crate) fn unload_inactive_tabs(
        &mut self,
        window: &ServoShellWindow,
        keep: &HashSet<WebViewId>,
        low_memory: bool,
    ) -> usize {
        self.sync_workspaces(window);
        let (minutes, on_low_memory) = {
            let settings = self.settings.borrow();
            (settings.tab_unload_minutes, settings.unload_on_low_memory)
        };
        let candidates = self.workspaces.unload_candidates(window, keep);
        let mut unloaded = 0;
        // STILLWATER_TAB_UNLOAD_SECONDS overrides the setting (used to test the unloader).
        let limit = std::env::var("STILLWATER_TAB_UNLOAD_SECONDS")
            .ok()
            .and_then(|seconds| seconds.parse().ok())
            .unwrap_or(minutes as u64 * 60);
        if limit > 0 {
            let limit = std::time::Duration::from_secs(limit);
            for (webview, idle) in &candidates {
                if *idle >= limit && self.unload_tab(window, *webview) {
                    unloaded += 1;
                }
            }
        }
        // Under memory pressure, also unload the least recently used tab that has been in the
        // background for at least a minute (one per check, to avoid unloading everything).
        if unloaded == 0 && low_memory && on_low_memory {
            if let Some((webview, _)) = candidates
                .iter()
                .find(|(_, idle)| *idle >= std::time::Duration::from_secs(60))
            {
                if self.unload_tab(window, *webview) {
                    unloaded += 1;
                }
            }
        }
        if unloaded > 0 {
            window.set_needs_update();
        }
        unloaded
    }

    /// Act on messages sent by the sidebar page. Returns true if the layout may have changed.
    pub(crate) fn handle_messages(&mut self, window: &ServoShellWindow) -> bool {
        let messages = std::mem::take(&mut *self.inbox.borrow_mut());
        if messages.is_empty() {
            return false;
        }
        self.sync_workspaces(window);
        let mut layout_changed = false;
        for message in messages {
            let (command, argument) = message.split_once('\t').unwrap_or((&message, ""));
            // Workspace commands take `<workspace id>[\t<text>]`.
            let (workspace, text) = argument.split_once('\t').unwrap_or((argument, ""));
            let workspace: u32 = workspace.parse().unwrap_or(0);
            match command {
                "ready" => {
                    self.last_pushed_state = None;
                    self.ready = true;
                    if std::env::var_os("STILLWATER_OPEN_SETTINGS").is_some() {
                        self.call("openSettings()");
                    }
                    // STILLWATER_TEST_SCRIPT: run JavaScript in the sidebar page once it's ready, to
                    // drive the UI in tests the same way clicks do.
                    if let Ok(script) = std::env::var("STILLWATER_TEST_SCRIPT") &&
                        let Some(webview) = &self.webview
                    {
                        webview.evaluate_javascript(script, |_| {});
                    }
                    crate::desktop::diagnostics::log("sidebar page ready");
                },
                "go" => {
                    // Search with the engine chosen in settings.
                    let template = self.settings.borrow().search_url_template();
                    let target = crate::parser::location_bar_input_to_url(argument, template)
                        .map(|url| url.to_string())
                        .unwrap_or_else(|| argument.to_owned());
                    match (&self.pane_focus, url::Url::parse(&target)) {
                        (Some(pane), Ok(url)) => pane.load(url),
                        _ => {
                            if let Some(active) = window.active_webview() &&
                                let Ok(url) = url::Url::parse(&target) &&
                                !is_same_document(active.url(), &url)
                            {
                                navigation_started(active.id());
                            }
                            window.queue_user_interface_command(UserInterfaceCommand::Go(target))
                        },
                    }
                    self.set_focused(false, window);
                },
                // Navigation acts on the focused split / Glance pane, if there is one.
                "back" => match &self.pane_focus {
                    Some(pane) => {
                        pane.go_back(1);
                    },
                    None => window.queue_user_interface_command(UserInterfaceCommand::Back),
                },
                "forward" => match &self.pane_focus {
                    Some(pane) => {
                        pane.go_forward(1);
                    },
                    None => window.queue_user_interface_command(UserInterfaceCommand::Forward),
                },
                "reload" => match &self.pane_focus {
                    Some(pane) => pane.reload(),
                    None => {
                        if let Some(active) = window.active_webview() {
                            navigation_started(active.id());
                        }
                        window.queue_user_interface_command(UserInterfaceCommand::Reload)
                    },
                },
                // Open a tab in split view (the tab moves there) or a pinned tab's page.
                "split-tab" if self.unloaded_tab(argument).is_some() => {
                    if let Some(url) = self
                        .unloaded_tab(argument)
                        .and_then(|key| self.workspaces.take_unloaded_url(key))
                    {
                        self.window_actions.push(WindowAction::OpenPane(PaneKind::Split, url));
                    }
                },
                "split-tab" => {
                    if let Some(id) = self.webview_for_tab(window, argument) &&
                        let Some(url) = window.webview_by_id(id).and_then(|webview| webview.url())
                    {
                        if window.active_webview().is_some_and(|active| active.id() == id) {
                            // Splitting the active tab: keep it, and show a copy beside it.
                            self.window_actions.push(WindowAction::OpenPane(PaneKind::Split, url));
                        } else {
                            self.workspaces.close_tab(window, id);
                            self.window_actions.push(WindowAction::OpenPane(PaneKind::Split, url));
                        }
                    }
                },
                "split-pin" => {
                    if let Some(url) = self.workspaces.pin_url(workspace) {
                        self.window_actions.push(WindowAction::OpenPane(PaneKind::Split, url));
                    }
                },
                // Preview a link in Glance (Alt+click does this from the page).
                "glance-open" => {
                    if let Ok(url) = url::Url::parse(argument) {
                        self.window_actions.push(WindowAction::OpenPane(PaneKind::Glance, url));
                    }
                },
                // The text fields' right-click menu (the page has no clipboard access of its own):
                // Copy and Cut put text on the clipboard; opening the menu asks for its text, for
                // Paste.
                "clip-copy" => {
                    if let Ok(mut clipboard) = arboard::Clipboard::new() {
                        let _ = clipboard.set_text(argument.to_owned());
                    }
                },
                "clip-read" => {
                    let text = arboard::Clipboard::new()
                        .ok()
                        .and_then(|mut clipboard| clipboard.get_text().ok())
                        .filter(|text| !text.is_empty());
                    self.call(&format!(
                        "clipboardText({})",
                        serde_json::to_string(&text).unwrap_or_else(|_| "null".into())
                    ));
                },
                // Tests: write a value from the sidebar page to the diagnostic log.
                "debug" => crate::desktop::diagnostics::log(format!("sidebar debug: {argument}")),
                // Tests: run JavaScript in the current tab and log what it returns.
                "debug-eval" => {
                    if let Some(webview) = window.active_webview() {
                        webview.evaluate_javascript(argument.to_owned(), |result| {
                            crate::desktop::diagnostics::log(format!("page eval: {result:?}"))
                        });
                    }
                },
                "adblock-site" => self.window_actions.push(WindowAction::AdblockToggleSite),
                "adblock-enabled" => self
                    .window_actions
                    .push(WindowAction::AdblockSetEnabled(argument == "1")),
                "split-close" => self.window_actions.push(WindowAction::ClosePane(PaneKind::Split)),
                "glance-close" => self.window_actions.push(WindowAction::ClosePane(PaneKind::Glance)),
                "new-tab" if self.settings.borrow().new_tab_bar => {
                    self.window_actions.push(WindowAction::CommandBar)
                },
                "set-newtab" => self.settings.borrow_mut().set_new_tab_bar(argument == "1"),
                "set-splash" => self.settings.borrow_mut().set_startup_animation(argument == "1"),
                "set-scrollbar" => self.settings.borrow_mut().set_scrollbar(&argument),
                "set-spaces" => {
                    let enabled = argument == "1";
                    self.settings.borrow_mut().set_spaces(enabled);
                    self.workspaces.set_enabled(enabled);
                    layout_changed = true;
                },
                "new-tab" => {
                    self.workspaces.open_tab(window);
                    self.focus_url(window);
                },
                "activate" => {
                    if let Some(id) = self.webview_for_tab(window, argument) {
                        window.activate_webview(id);
                    } else if let Some(key) = self.unloaded_tab(argument) {
                        self.workspaces.activate_entry(window, TabEntry::Unloaded(key));
                    }
                },
                "close" => {
                    if let Some(id) = self.webview_for_tab(window, argument) {
                        self.remember_closed(window, TabEntry::Live(id));
                        self.workspaces.close_tab(window, id);
                    } else if let Some(key) = self.unloaded_tab(argument) {
                        self.remember_closed(window, TabEntry::Unloaded(key));
                        self.workspaces.close_unloaded(key);
                    }
                },
                // The tab menu's Reload and Duplicate.
                "tab-reload" => {
                    if let Some(id) = self.webview_for_tab(window, argument) &&
                        let Some(webview) = window.webview_by_id(id)
                    {
                        navigation_started(id);
                        webview.reload();
                    }
                },
                "tab-duplicate" => {
                    let url = match self.webview_for_tab(window, argument) {
                        Some(id) => window.webview_by_id(id).and_then(|webview| webview.url()),
                        None => self
                            .unloaded_tab(argument)
                            .and_then(|key| self.workspaces.unloaded_json(key))
                            .and_then(|tab| url::Url::parse(tab["url"].as_str()?).ok()),
                    };
                    if let Some(url) = url {
                        window.queue_user_interface_command(UserInterfaceCommand::NewWebViewWithUrl(
                            url,
                        ));
                    }
                },
                "tab-unload" => {
                    if let Some(id) = self.webview_for_tab(window, argument) {
                        self.unload_tab(window, id);
                    }
                },
                "close-active" => {
                    if let Some(active) = window.active_webview() {
                        self.remember_closed(window, TabEntry::Live(active.id()));
                        self.workspaces.close_tab(window, active.id());
                    }
                },
                "tab-index" => self
                    .workspaces
                    .activate_tab_index(window, argument.parse().unwrap_or(usize::MAX)),
                "tab-next" => self.workspaces.cycle_tab(window, 1),
                "tab-prev" => self.workspaces.cycle_tab(window, -1),
                "ws-switch" => self.workspaces.switch_to(window, workspace),
                "ws-index" => self
                    .workspaces
                    .switch_to_index(window, argument.parse().unwrap_or(0)),
                "ws-new" => {
                    self.workspaces.create(window);
                    self.last_pushed_state = None;
                },
                "ws-rename" => self.workspaces.rename(workspace, text),
                "ws-icon" => self.workspaces.cycle_icon(workspace),
                "ws-color" => self.workspaces.cycle_color(workspace),
                "ws-delete" => self.workspaces.delete(window, workspace),
                // Pinned tabs, Essentials and folders. workspace holds the first numeric
                // argument (a tab, pin or folder id) and 	ext the rest.
                "pin" | "essential" => {
                    if let Some(id) = self.webview_for_tab(window, &workspace.to_string()) {
                        self.workspaces.pin_tab(window, id, command == "essential");
                    } else if let Some(key) = self.unloaded_tab(&workspace.to_string()) {
                        self.workspaces.pin_unloaded(key, command == "essential");
                    }
                },
                "unpin" => self.workspaces.unpin(workspace),
                "pin-essential" => self.workspaces.toggle_essential(workspace),
                "pin-open" => self.workspaces.open_pin(window, workspace),
                "pin-unload" => self.workspaces.unload_pin(window, workspace),
                "pin-reset" => self.workspaces.reset_pin(window, workspace),
                "pin-rename" => self.workspaces.rename_pin(workspace, text),
                "pin-folder" => self.workspaces.move_pin_to_folder(workspace, text.parse().ok()),
                "pin-move" => self.workspaces.move_pin(workspace, text.parse().unwrap_or(0)),
                "tab-rename" => {
                    if let Some(id) = self.webview_for_tab(window, &workspace.to_string()) {
                        self.workspaces.rename_tab(id, text);
                    } else if let Some(key) = self.unloaded_tab(&workspace.to_string()) {
                        self.workspaces.rename_unloaded(key, text);
                    }
                },
                "tab-move-space" => {
                    if let (Some(id), Ok(space)) =
                        (self.webview_for_tab(window, &workspace.to_string()), text.parse())
                    {
                        self.workspaces.move_tab_to_space(window, id, space);
                    } else if let (Some(key), Ok(space)) =
                        (self.unloaded_tab(&workspace.to_string()), text.parse())
                    {
                        self.workspaces.move_unloaded_to_space(key, space);
                    }
                },
                "folder-new" => {
                    let folder = self.workspaces.create_folder();
                    // Optionally move a pinned tab straight into the new folder.
                    if workspace != 0 {
                        self.workspaces.move_pin_to_folder(workspace, Some(folder));
                    }
                },
                "folder-rename" => self.workspaces.rename_folder(workspace, text),
                "folder-toggle" => self.workspaces.toggle_folder(workspace),
                "folder-delete" => self.workspaces.delete_folder(workspace),
                "toggle-compact" => {
                    self.toggle_compact();
                    layout_changed = true;
                },
                "blur" => self.set_focused(false, window),
                "win-min" => self.window_actions.push(WindowAction::Minimize),
                "win-max" => self.window_actions.push(WindowAction::ToggleMaximize),
                "win-close" => self.window_actions.push(WindowAction::Close),
                "win-drag" => self.window_actions.push(WindowAction::Drag),
                "update" => self.window_actions.push(WindowAction::Update),
                // The settings page, in a tab: `[<category>]`.
                "open-settings" => self.open_settings(window, argument),
                "report-site" => {
                    let tab = self.webview_for_tab(window, argument);
                    self.window_actions.push(WindowAction::ReportSite(tab));
                },
                "check-updates" => {
                    self.update_check = Some("Checking…".into());
                    self.window_actions.push(WindowAction::CheckForUpdates);
                },
                "set-theme" => {
                    if let Some(choice) = ThemeChoice::from_id(argument) {
                        self.settings.borrow_mut().set_theme(choice);
                    }
                },
                "set-search" => self.settings.borrow_mut().set_search_engine(argument),
                "set-tab-unload" => {
                    if let Ok(minutes) = argument.parse() {
                        self.settings.borrow_mut().set_tab_unload_minutes(minutes);
                    }
                },
                "pw-save" => self.password(PasswordAction::SaveOffer),
                "pw-never" => self.password(PasswordAction::NeverForSite),
                "pw-dismiss" => self.password(PasswordAction::DismissOffer),
                "pw-fill" | "pw-copy-user" | "pw-copy" | "pw-reveal" | "pw-delete" => {
                    if let Ok(id) = argument.parse() {
                        self.password(match command {
                            "pw-fill" => PasswordAction::Fill(id),
                            "pw-copy-user" => PasswordAction::Copy(id, false),
                            "pw-copy" => PasswordAction::Copy(id, true),
                            "pw-reveal" => PasswordAction::Reveal(id),
                            _ => PasswordAction::Delete(id),
                        });
                    }
                },
                "pw-autofill" => self.password(PasswordAction::SetAutofill(argument == "1")),
                "pw-forget-never" => self.password(PasswordAction::ForgetNever(argument.to_owned())),
                // Tests: a key pressed in the saved-login list under the focused field.
                "pw-chooser-key" => self.password(PasswordAction::ChooserKey(argument.to_owned())),
                "compare" => self.window_actions.push(WindowAction::Compare),
                "zen-scan" => self
                    .window_actions
                    .push(WindowAction::ZenScan(argument.parse().unwrap_or(0))),
                // `<profile index>\t<what>`, what = comma-separated pins, tabs, passwords.
                "zen-import" => {
                    let (profile, what) = argument.split_once('\t').unwrap_or((argument, ""));
                    let what: Vec<&str> = what.split(',').collect();
                    self.window_actions.push(WindowAction::ZenImport {
                        profile: profile.parse().unwrap_or(0),
                        pins: what.contains(&"pins"),
                        tabs: what.contains(&"tabs"),
                        passwords: what.contains(&"passwords"),
                        history: what.contains(&"history"),
                        bookmarks: what.contains(&"bookmarks"),
                        cookies: what.contains(&"cookies"),
                    });
                },
                // History panel (Ctrl+H) and bookmarking the page (Ctrl+D): show the sidebar.
                "hist-show" | "bm-star" if self.compact.get() => {
                    self.peek.set(true);
                    layout_changed = true;
                    self.window_actions
                        .push(WindowAction::History(command.to_owned(), argument.to_owned()));
                },
                _ if command.starts_with("hist-") || command.starts_with("bm-") => self
                    .window_actions
                    .push(WindowAction::History(command.to_owned(), argument.to_owned())),
                // Ctrl+J: show the Downloads panel.
                "dl-panel" => {
                    if self.compact.get() {
                        self.peek.set(true);
                    }
                    self.call("openDownloads()");
                    layout_changed = true;
                },
                command if command.starts_with("dl-") => self
                    .window_actions
                    .push(WindowAction::Download(command.to_owned(), argument.to_owned())),
                command if command.starts_with("tr-") => self
                    .window_actions
                    .push(WindowAction::Translate(command.to_owned(), argument.to_owned())),
                "set-unload-low-memory" => self
                    .settings
                    .borrow_mut()
                    .set_unload_on_low_memory(argument == "1"),
                command if command.starts_with("session-") => {
                    self.session_command(window, command, argument)
                },
                command if command.starts_with("find-") => {
                    if let Some(find) = FindCommand::parse(command, argument) {
                        self.window_actions.push(WindowAction::Find(find));
                    }
                },
                _ => warn!("unknown sidebar message: {message}"),
            }
            window.set_needs_update();
        }
        layout_changed
    }

    fn password(&mut self, action: PasswordAction) {
        self.window_actions.push(WindowAction::Password(action));
    }

    /// Send the current tab list and navigation state to the page, if it changed.
    pub(crate) fn push_state(&mut self, window: &ServoShellWindow) {
        if self.webview.is_none() {
            return;
        }
        self.sync_workspaces(window);
        let webviews = window.webviews();
        self.tab_ids
            .retain(|id, _| webviews.iter().any(|(other, _)| other == id));
        self.favicons
            .retain(|id, _| webviews.iter().any(|(other, _)| other == id));

        let active = window.active_webview();
        let active_id = active.as_ref().map(WebView::id);
        // The URL bar and navigation buttons follow the focused split / Glance pane.
        let navigated = self.pane_focus.clone().or_else(|| active.clone());
        let pane_json = |pane: &Option<(String, String)>| {
            pane.as_ref()
                .map(|(title, url)| serde_json::json!({ "title": title, "url": url }))
        };
        let split = pane_json(&self.panes.0);
        let glance = pane_json(&self.panes.1);
        self.workspaces.refresh_pins(window, &self.favicons);
        let entries = self.workspaces.regular_entries(window);
        let mut tabs = Vec::with_capacity(entries.len());
        for entry in entries {
            match entry {
                TabEntry::Live(id) => {
                    let Some((_, webview)) = webviews.iter().find(|(other, _)| *other == id) else {
                        continue;
                    };
                    let tab_id = self.tab_key(id);
                    tabs.push(serde_json::json!({
                        "id": tab_id,
                        "title": self.workspaces.custom_title(id).cloned().or_else(|| webview.page_title()).unwrap_or_default(),
                        "url": webview.url().map(|url| url.to_string())
                            .or_else(|| self.workspaces.requested_url(id).cloned())
                            .unwrap_or_default(),
                        "active": Some(id) == active_id,
                        "loading": is_navigating(webview),
                        "favicon": self.favicons.get(&id),
                        "loaded": true,
                        "playing": self.playing_media.contains(&id),
                    }));
                },
                TabEntry::Unloaded(key) => tabs.extend(self.workspaces.unloaded_json(key)),
            }
        }
        let [r, g, b] = self.background();
        let background = format!("rgb({r}, {g}, {b})");
        let state = serde_json::json!({
            "tabs": tabs,
            "essentials": self.workspaces.essentials_json(window, active_id),
            "pinned": self.workspaces.pinned_json(window, active_id),
            "url": navigated.as_ref().and_then(|webview| webview.url()).map(|url| url.to_string()).unwrap_or_default(),
            "canGoBack": navigated.as_ref().is_some_and(|webview| webview.can_go_back()),
            "canGoForward": navigated.as_ref().is_some_and(|webview| webview.can_go_forward()),
            "split": split,
            "glance": glance,
            "paneFocused": self.pane_focus.is_some(),
            "adblock": self.adblock,
            "translate": self.translate,
            "passwords": self.passwords,
            "zen": self.zen,
            "compare": self.compare,
            "session": self.session_state(),
            "hist": self.hist,
            "commandBar": self.command_bar,
            "downloads": self.downloads,
            "compact": self.compact.get(),
            "workspaces": self.workspaces.to_json(),
            "background": background,
            "frameless": self.frameless,
            "settings": self.settings.borrow().to_json(),
            "updateCheck": self.update_check,
            "maximized": self.maximized,
            "update": self.update.as_ref().map(|update| serde_json::json!({
                "version": update.version,
                "installable": update.installable,
                "status": match &self.update_status {
                    None => "available".to_owned(),
                    Some(UpdateStatus::Downloading) => "downloading".to_owned(),
                    Some(UpdateStatus::Installing) => "installing".to_owned(),
                    Some(UpdateStatus::Failed(error)) => format!("failed: {error}"),
                },
            })),
        })
        .to_string();

        // Settings pages get the same state (it has no passwords, only the list of logins).
        self.settings_pages.retain(|(page, _)| {
            webviews.iter().any(|(id, _)| *id == page.id()) && page.url().is_some_and(|url| is_settings_url(&url))
        });
        for (page, last) in &mut self.settings_pages {
            if last.as_ref() != Some(&state) {
                page.evaluate_javascript(
                    format!("window.stillwaterSettings && window.stillwaterSettings.setState({state})"),
                    |_| {},
                );
                *last = Some(state.clone());
            }
        }
        if self.last_pushed_state.as_ref() != Some(&state) {
            self.call(&format!("setState({state})"));
            self.last_pushed_state = Some(state);
        }
        self.save_session(window);
    }
}

/// Encode RGBA pixels as a `data:image/png;base64,...` URL for use in the sidebar page.
pub(crate) fn png_data_url(width: u32, height: u32, rgba: Vec<u8>) -> Option<String> {
    let image = image::RgbaImage::from_raw(width, height, rgba)?;
    let mut png = std::io::Cursor::new(Vec::new());
    image.write_to(&mut png, image::ImageFormat::Png).ok()?;
    Some(format!("data:image/png;base64,{}", base64(png.get_ref())))
}

fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = (chunk[0] as u32) << 16 |
            (*chunk.get(1).unwrap_or(&0) as u32) << 8 |
            *chunk.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(TABLE[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}
