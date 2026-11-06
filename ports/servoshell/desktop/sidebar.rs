/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! A vertical-tab sidebar whose UI is an HTML page rendered by Servo itself
//! (`servo:sidebar/index.html`), styled after Zen Browser.
//!
//! Page -> Rust: `console.log("__sidebar__<command>\t<argument>")`.
//! Rust -> Page: `window.sidebar.setState(<json>)` via `evaluate_javascript`.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
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
use crate::desktop::settings::{Settings, ThemeChoice};
use crate::desktop::updater::{UpdateInfo, UpdateStatus};
use crate::desktop::workspaces::Workspaces;
use crate::running_app_state::{RunningAppState, UserInterfaceCommand};
use crate::window::ServoShellWindow;

/// Width of the expanded sidebar, in device-independent pixels.
pub(crate) const SIDEBAR_WIDTH: f32 = 240.0;
/// Gap between the window edge / sidebar and the rounded web content card.
pub(crate) const CONTENT_MARGIN: f32 = 8.0;
/// Corner radius of the web content card.
pub(crate) const CONTENT_RADIUS: f32 = 8.0;
/// In compact mode, hovering within this many pixels of the left edge reveals the sidebar.
const PEEK_EDGE: f32 = 10.0;

const MESSAGE_PREFIX: &str = "__sidebar__";
const SIDEBAR_URL: &str = "servo:sidebar/index.html";

/// Window operations requested by the sidebar's own caption buttons.
#[derive(Clone, Copy, Debug)]
pub(crate) enum WindowAction {
    Minimize,
    ToggleMaximize,
    Close,
    Drag,
    /// Install the available update (or open its release page for portable copies).
    Update,
    /// Check GitHub for an update right now (settings panel).
    CheckForUpdates,
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
}

impl Sidebar {
    pub(crate) fn new(
        rendering_context: Rc<OffscreenRenderingContext>,
        waker: RepaintEventLoopWaker,
    ) -> Self {
        let settings = Settings::load();
        Self {
            rendering_context,
            webview: None,
            inbox: Default::default(),
            cursor: Default::default(),
            waker: Waker::from(Arc::new(waker)),
            last_pushed_state: None,
            tab_ids: Default::default(),
            next_tab_id: 1,
            favicons: Default::default(),
            // STILLWATER_START_COMPACT: start with the sidebar collapsed (used for testing).
            compact: Cell::new(
                settings.compact || std::env::var_os("STILLWATER_START_COMPACT").is_some(),
            ),
            peek: Cell::new(false),
            focused: Cell::new(false),
            workspaces: Workspaces::load(),
            theme: Theme::Light,
            window_actions: Vec::new(),
            frameless: false,
            maximized: false,
            update: None,
            update_status: None,
            ready: false,
            settings: RefCell::new(settings),
            update_check: None,
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

    /// Queue a command as if the sidebar page had sent it (used by keyboard shortcuts).
    pub(crate) fn post(&self, message: &str) {
        self.inbox.borrow_mut().push(message.to_owned());
        self.waker.wake_by_ref();
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
        if self.compact.get() {
            let content = full.shrink(m);
            let sidebar = self.peek.get().then(|| {
                egui::Rect::from_min_max(
                    egui::pos2(full.min.x + m, full.min.y + m),
                    egui::pos2(full.min.x + m + SIDEBAR_WIDTH, full.max.y - m),
                )
            });
            (sidebar, content)
        } else {
            let sidebar = egui::Rect::from_min_max(
                full.min,
                egui::pos2(full.min.x + SIDEBAR_WIDTH, full.max.y),
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
            point.x <= CONTENT_MARGIN + SIDEBAR_WIDTH + 12.0
        } else {
            point.x <= PEEK_EDGE
        };
        self.peek.replace(peek) != peek
    }

    pub(crate) fn is_focused(&self) -> bool {
        self.focused.get()
    }

    /// Move keyboard focus between the sidebar and the active web content.
    pub(crate) fn set_focused(&self, focused: bool, window: &ServoShellWindow) {
        if self.focused.replace(focused) == focused {
            return;
        }
        if let Some(webview) = &self.webview {
            webview.set_focused(focused);
        }
        if let Some(active) = window.active_webview() {
            active.set_focused(!focused);
        }
    }

    fn call(&self, script: &str) {
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

    /// Act on messages sent by the sidebar page. Returns true if the layout may have changed.
    pub(crate) fn handle_messages(&mut self, window: &ServoShellWindow) -> bool {
        let messages = std::mem::take(&mut *self.inbox.borrow_mut());
        if messages.is_empty() {
            return false;
        }
        self.workspaces.sync(window);
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
                    window.queue_user_interface_command(UserInterfaceCommand::Go(target));
                    self.set_focused(false, window);
                },
                "back" => window.queue_user_interface_command(UserInterfaceCommand::Back),
                "forward" => window.queue_user_interface_command(UserInterfaceCommand::Forward),
                "reload" => window.queue_user_interface_command(UserInterfaceCommand::Reload),
                "new-tab" => {
                    self.workspaces.open_tab(window);
                    self.focus_url(window);
                },
                "activate" => {
                    if let Some(id) = self.webview_for_tab(window, argument) {
                        window.activate_webview(id);
                    }
                },
                "close" => {
                    if let Some(id) = self.webview_for_tab(window, argument) {
                        self.workspaces.close_tab(window, id);
                    }
                },
                "close-active" => {
                    if let Some(active) = window.active_webview() {
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
                    }
                },
                "tab-move-space" => {
                    if let (Some(id), Ok(space)) =
                        (self.webview_for_tab(window, &workspace.to_string()), text.parse())
                    {
                        self.workspaces.move_tab_to_space(window, id, space);
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
                "open-settings" => {
                    if self.compact.get() {
                        self.peek.set(true);
                    }
                    self.call("openSettings()");
                    layout_changed = true;
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
                _ => warn!("unknown sidebar message: {message}"),
            }
            window.set_needs_update();
        }
        layout_changed
    }

    /// Send the current tab list and navigation state to the page, if it changed.
    pub(crate) fn push_state(&mut self, window: &ServoShellWindow) {
        if self.webview.is_none() {
            return;
        }
        self.workspaces.sync(window);
        let webviews = window.webviews();
        self.tab_ids
            .retain(|id, _| webviews.iter().any(|(other, _)| other == id));
        self.favicons
            .retain(|id, _| webviews.iter().any(|(other, _)| other == id));

        let active = window.active_webview();
        let active_id = active.as_ref().map(WebView::id);
        self.workspaces.refresh_pins(window, &self.favicons);
        let current_tabs = self.workspaces.regular_tabs(window);
        let mut tabs = Vec::with_capacity(current_tabs.len());
        for (id, webview) in webviews.iter().filter(|(id, _)| current_tabs.contains(id)) {
            let tab_id = *self.tab_ids.entry(*id).or_insert_with(|| {
                self.next_tab_id += 1;
                self.next_tab_id
            });
            tabs.push(serde_json::json!({
                "id": tab_id,
                "title": self.workspaces.custom_title(*id).cloned().or_else(|| webview.page_title()).unwrap_or_default(),
                "url": webview.url().map(|url| url.to_string()).unwrap_or_default(),
                "active": Some(*id) == active_id,
                "loading": matches!(webview.load_status(), LoadStatus::Started | LoadStatus::HeadParsed),
                "favicon": self.favicons.get(id),
            }));
        }
        let [r, g, b] = self.background();
        let background = format!("rgb({r}, {g}, {b})");
        let state = serde_json::json!({
            "tabs": tabs,
            "essentials": self.workspaces.essentials_json(window, active_id),
            "pinned": self.workspaces.pinned_json(window, active_id),
            "url": active.as_ref().and_then(|webview| webview.url()).map(|url| url.to_string()).unwrap_or_default(),
            "canGoBack": active.as_ref().is_some_and(|webview| webview.can_go_back()),
            "canGoForward": active.as_ref().is_some_and(|webview| webview.can_go_forward()),
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

        if self.last_pushed_state.as_ref() != Some(&state) {
            self.call(&format!("setState({state})"));
            self.last_pushed_state = Some(state);
        }
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
