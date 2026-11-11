/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

#[cfg(any(
    target_os = "windows",
    target_os = "linux",
    target_os = "freebsd",
    target_os = "macos"
))]
use std::fs;
#[cfg(any(
    target_os = "windows",
    target_os = "linux",
    target_os = "freebsd",
    target_os = "macos"
))]
use std::path::Path;
use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::Arc;

use accesskit::Affine;
use dpi::PhysicalSize;
use egui::{
    Color32, CornerRadius, FontDefinitions, Id, Label, LayerId, Order, PaintCallback, Stroke,
    StrokeKind, pos2,
};
#[cfg(any(
    target_os = "windows",
    target_os = "linux",
    target_os = "freebsd",
    target_os = "macos"
))]
use egui::{FontData, FontFamily};
use egui_glow::{CallbackFn, EguiGlow};
use egui_winit::EventResponse;
use euclid::{Point2D, Rect, Scale, Size2D, Vector2D};
#[cfg(any(
    target_os = "windows",
    target_os = "linux",
    target_os = "freebsd",
    target_os = "macos"
))]
use log::info;
use servo::{
    DeviceIndependentPixel, DevicePixel, Image, LoadStatus, OffscreenRenderingContext, PixelFormat,
    RenderingContext, WebViewId, WindowRenderingContext,
};
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, EventLoopProxy};
use winit::window::Window;

use crate::desktop::event_loop::{AppEvent, RepaintEventLoopWaker};
use crate::desktop::find_bar::FindBar;
use crate::desktop::panes::{Pane, PaneKind};
use crate::desktop::headed_window;
use crate::desktop::sidebar::{CONTENT_RADIUS, Sidebar, WindowAction, png_data_url};
use crate::desktop::report;
use crate::desktop::zen_import;

/// A change to the split view / Glance panes, applied on the next update.
pub(crate) enum PaneRequest {
    Open(PaneKind, url::Url),
    Close(PaneKind),
    GlanceToTab,
    GlanceToSplit,
}
use crate::desktop::updater::{self, UpdateStatus};
use crate::running_app_state::{RunningAppState, UserInterfaceCommand};
use crate::window::{PlatformWindow, ServoShellWindow};

/// The user interface of a headed servoshell. egui is only used as a thin compositor
/// (plus native dialogs); the browser chrome itself is the HTML [`Sidebar`].
pub struct Gui {
    rendering_context: Rc<OffscreenRenderingContext>,
    context: EguiGlow,

    /// The Zen-style sidebar, rendered by Servo.
    pub(crate) sidebar: Sidebar,

    /// Where the sidebar was last drawn, in device-independent pixels, if visible.
    sidebar_rect: Option<egui::Rect>,

    /// Where the web content was last drawn, in device-independent pixels.
    content_rect: egui::Rect,

    /// The text to display in the status bar on the bottom of the window.
    status_text: Option<String>,

    /// AccessKit tree updates pending the next egui tick.
    /// This allows us to ensure that graft nodes are sent before the subtrees they graft.
    pending_accesskit_updates: Vec<accesskit::TreeUpdate>,

    /// Whether the window has been shown yet (it stays hidden until its first frame).
    shown: bool,

    /// Zen's hover-reveal top bar with the window buttons, while it's showing.
    top_bar_rect: Option<egui::Rect>,
    /// Where the top bar is (shown or not) while the window is frameless.
    top_bar_zone: Option<egui::Rect>,

    /// Needed to create render surfaces for extra panes.
    window_rendering_context: Rc<WindowRenderingContext>,
    /// Split view's second page, beside the active tab.
    split: Option<Pane>,
    /// Glance: a link preview floating over the page.
    glance: Option<Pane>,
    /// Zen's floating address bar for a new tab.
    pub(crate) command_bar: crate::desktop::command_bar::CommandBar,
    /// Pane changes requested since the last update (applied in `update`).
    pane_requests: RefCell<Vec<PaneRequest>>,
    /// Which pane has keyboard focus (`None` = the active tab).
    pane_focus: Cell<Option<PaneKind>>,
    split_rect: Option<egui::Rect>,
    glance_rect: Option<egui::Rect>,
    /// Where the pane buttons are drawn, so clicks there go to egui.
    pane_control_rects: RefCell<Vec<egui::Rect>>,
    /// Tabs whose page has drawn at least once (their surfaces start out white).
    painted_webviews: HashSet<WebViewId>,
    /// The find bar (Ctrl+F).
    pub(crate) find_bar: FindBar,
    /// Where the saved-login list under a focused sign-in field is drawn, if it shows.
    password_chooser_rect: Option<egui::Rect>,
    /// The startup animation, until it has finished (or was never wanted).
    splash: Option<crate::desktop::splash::Splash>,
    splash_decided: bool,
    /// The page scrollbars drawn over each page view.
    pub(crate) scrollbars: RefCell<crate::desktop::scrollbar::Scrollbars>,
}

#[cfg(any(
    target_os = "windows",
    target_os = "linux",
    target_os = "freebsd",
    target_os = "macos"
))]
/// Map a font file into memory for the life of the process. The CJK fallback fonts are big
/// (msyh.ttc alone is ~19 MB); reading them made two private copies (egui clones owned font
/// data), while mapped pages are shared with the file cache and only loaded when used.
fn map_font_file(path: &Path) -> std::io::Result<&'static [u8]> {
    let file = fs::File::open(path)?;
    // SAFETY: system font files aren't modified while they're in use.
    let map = unsafe { memmap2::Mmap::map(&file)? };
    Ok(Box::leak(Box::new(map)))
}

#[cfg(any(
    target_os = "windows",
    target_os = "linux",
    target_os = "freebsd",
    target_os = "macos"
))]
fn load_cjk_fonts(font_candidates: &[(&str, &str)]) -> FontDefinitions {
    let mut fonts = FontDefinitions::default();
    let mut loaded_font_names = Vec::new();

    for (path_str, font_name) in font_candidates.iter() {
        let font_path = Path::new(path_str);
        if font_path.exists() {
            match map_font_file(font_path) {
                Ok(bytes) => {
                    if !fonts.font_data.contains_key(*font_name) {
                        fonts
                            .font_data
                            .insert(font_name.to_string(), Arc::new(FontData::from_static(bytes)));
                        loaded_font_names.push(font_name.to_string());
                        info!("Loaded font: {}", font_name);
                    }
                },
                Err(error) => {
                    info!("Failed to read font {}: {}", font_name, error);
                },
            }
        }
    }

    if !loaded_font_names.is_empty() &&
        let Some(proportional) = fonts.families.get_mut(&FontFamily::Proportional)
    {
        proportional.append(&mut loaded_font_names);
    }

    fonts
}

#[cfg(target_os = "windows")]
fn configure_fonts() -> FontDefinitions {
    load_cjk_fonts(&[
        (r"C:\Windows\Fonts\malgun.ttf", "Malgun Gothic"), // Korean
        (r"C:\Windows\Fonts\msyh.ttc", "Microsoft YaHei"), // Chinese + Japanese
    ])
}

#[cfg(any(target_os = "linux", target_os = "freebsd"))]
fn configure_fonts() -> FontDefinitions {
    load_cjk_fonts(&[
        (
            "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
            "Noto Sans CJK",
        ), // Ubuntu/Debian
        (
            "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
            "Noto Sans CJK",
        ), // Fedora/Arch
        // FreeBSD splits the Noto CJK fonts into regional subsets
        (
            "/usr/local/share/fonts/noto/NotoSansCJKhk-Regular.otf",
            "Noto Sans CJK HK",
        ),
        (
            "/usr/local/share/fonts/noto/NotoSansCJKjp-Regular.otf",
            "Noto Sans CJK JP",
        ),
        (
            "/usr/local/share/fonts/noto/NotoSansCJKkr-Regular.otf",
            "Noto Sans CJK KR",
        ),
        (
            "/usr/local/share/fonts/noto/NotoSansCJKsc-Regular.otf",
            "Noto Sans CJK SC",
        ),
        (
            "/usr/local/share/fonts/noto/NotoSansCJKtc-Regular.otf",
            "Noto Sans CJK TC",
        ),
        (
            "/usr/share/fonts/truetype/wqy/wqy-microhei.ttc",
            "WenQuanYi Micro Hei",
        ), // common fallback
        (
            "/usr/local/share/fonts/wqy/wqy-microhei.ttc",
            "WenQuanYi Micro Hei",
        ), // FreeBSD
    ])
}

#[cfg(target_os = "macos")]
fn configure_fonts() -> FontDefinitions {
    load_cjk_fonts(&[
        (
            "/System/Library/Fonts/AppleSDGothicNeo.ttc",
            "Apple SD Gothic Neo",
        ), // Korean
        ("/System/Library/Fonts/STHeiti Medium.ttc", "STHeiti Medium"), // Chinese + Japanese
    ])
}

impl Drop for Gui {
    fn drop(&mut self) {
        self.rendering_context
            .make_current()
            .expect("Could not make window RenderingContext current");
        self.context.destroy();
    }
}



impl Gui {
    pub(crate) fn new(
        winit_window: &Window,
        event_loop: &ActiveEventLoop,
        event_loop_proxy: EventLoopProxy<AppEvent>,
        rendering_context: Rc<OffscreenRenderingContext>,
        window_rendering_context: Rc<WindowRenderingContext>,
        sidebar: Sidebar,
    ) -> Self {
        rendering_context
            .make_current()
            .expect("Could not make window RenderingContext current");
        let mut context = EguiGlow::new(
            event_loop,
            rendering_context.glow_gl_api(),
            None,
            None,
            false,
        );

        let font_definitions = configure_fonts();
        context.egui_ctx.set_fonts(font_definitions);

        context
            .egui_winit
            .init_accesskit(event_loop, winit_window, event_loop_proxy);
        // The window is shown after its first frame is painted (see the paint method),
        // once it has real content, so it never flashes an empty white frame at startup.

        context.egui_ctx.options_mut(|options| {
            // Disable the builtin egui handlers for the Ctrl+Plus, Ctrl+Minus and Ctrl+0
            // shortcuts as they don't work well with servoshell's `device-pixel-ratio` CLI argument.
            options.zoom_with_keyboard = false;

            // On platforms where winit fails to obtain a system theme, fall back to a light theme
            // since it is the more common default.
            options.fallback_theme = egui::Theme::Light;
        });

        Self {
            rendering_context,
            context,
            sidebar,
            sidebar_rect: None,
            content_rect: egui::Rect::NOTHING,
            status_text: None,
            pending_accesskit_updates: vec![],
            shown: false,
            top_bar_rect: None,
            top_bar_zone: None,
            window_rendering_context,
            split: None,
            glance: None,
            command_bar: Default::default(),
            pane_requests: Default::default(),
            pane_focus: Cell::new(None),
            split_rect: None,
            glance_rect: None,
            pane_control_rects: Default::default(),
            painted_webviews: HashSet::new(),
            find_bar: FindBar::default(),
            password_chooser_rect: None,
            splash: Some(crate::desktop::splash::Splash::new()),
            splash_decided: false,
            scrollbars: Default::default(),
        }
    }

    pub(crate) fn has_keyboard_focus(&self) -> bool {
        self.context
            .egui_ctx
            .memory(|memory| memory.focused().is_some())
    }

    pub(crate) fn surrender_focus(&self) {
        self.context.egui_ctx.memory_mut(|memory| {
            if let Some(focused) = memory.focused() {
                memory.surrender_focus(focused);
            }
        });
    }

    pub(crate) fn on_window_event(
        &mut self,
        winit_window: &Window,
        event: &WindowEvent,
    ) -> EventResponse {
        self.context.on_window_event(winit_window, event)
    }

    /// The offset of the web content from the top-left of the window.
    pub(crate) fn content_origin(&self) -> Vector2D<f32, DeviceIndependentPixel> {
        if self.content_rect == egui::Rect::NOTHING {
            return Vector2D::zero();
        }
        Vector2D::new(self.content_rect.min.x, self.content_rect.min.y)
    }

    /// Return true iff the given position is not over the web content (i.e. over the margins
    /// around it, which belong to egui).
    pub(crate) fn is_outside_content_rect(
        &self,
        position: Point2D<f32, DeviceIndependentPixel>,
    ) -> bool {
        !self.content_rect.contains(pos2(position.x, position.y))
    }

    /// Whether `position` is on the docked sidebar's right edge, which drags to resize it:
    /// a 6 px zone straddling the sidebar/page boundary, below the top bar.
    pub(crate) fn sidebar_resize_handle_contains(
        &self,
        position: Point2D<f32, DeviceIndependentPixel>,
    ) -> bool {
        const HALF_GRAB: f32 = 3.0;
        let Some(rect) = self.sidebar_rect else {
            return false;
        };
        self.sidebar.is_resizable() &&
            !self.top_bar_contains(position) &&
            (position.x - rect.max.x).abs() <= HALF_GRAB &&
            position.y >= rect.min.y + crate::desktop::sidebar::CONTENT_MARGIN &&
            position.y <= rect.max.y - crate::desktop::sidebar::CONTENT_MARGIN
    }

    /// If `position` is over the visible sidebar, return it relative to the sidebar's origin.
    /// Whether `position` is over the top bar (window buttons), while it's showing, or in the
    /// strip along the top edge that reveals it (a press there may beat the frame that shows it).
    pub(crate) fn top_bar_contains(&self, position: Point2D<f32, DeviceIndependentPixel>) -> bool {
        self.top_bar_hit(position).is_some()
    }

    /// What a press at `position` on the top bar hits, if it is on the top bar.
    pub(crate) fn top_bar_hit(&self, position: Point2D<f32, DeviceIndependentPixel>) -> Option<TopBarHit> {
        let point = pos2(position.x, position.y);
        let bar = match (self.top_bar_rect, self.top_bar_zone) {
            (Some(bar), _) if bar.contains(point) => bar,
            (_, Some(zone)) if zone.contains(point) && point.y <= zone.min.y + TOP_BAR_REVEAL => zone,
            _ => return None,
        };
        let hit = [TopBarHit::Close, TopBarHit::Maximize, TopBarHit::Minimize]
            .into_iter()
            .enumerate()
            .find(|(index, _)| caption_button_rect(bar, *index).contains(point))
            .map_or(TopBarHit::Drag, |(_, hit)| hit);
        Some(hit)
    }

    /// Ask for a pane change; it happens on the next update.
    pub(crate) fn request_pane(&self, request: PaneRequest) {
        self.pane_requests.borrow_mut().push(request);
    }

    pub(crate) fn glance_open(&self) -> bool {
        self.glance.is_some()
    }

    fn pane(&self, kind: PaneKind) -> Option<&Pane> {
        match kind {
            PaneKind::Split => self.split.as_ref(),
            PaneKind::Glance => self.glance.as_ref(),
        }
    }

    /// The extra pane under `position` (Glance first), with the point relative to it.
    pub(crate) fn pane_at(
        &self,
        position: Point2D<f32, DeviceIndependentPixel>,
    ) -> Option<(PaneKind, Point2D<f32, DeviceIndependentPixel>)> {
        let point = pos2(position.x, position.y);
        let hit = |rect: Option<egui::Rect>, kind| {
            rect.filter(|rect| rect.contains(point))
                .map(|rect| (kind, Point2D::new(point.x - rect.min.x, point.y - rect.min.y)))
        };
        if self.glance.is_some() {
            return hit(self.glance_rect, PaneKind::Glance);
        }
        hit(self.split_rect, PaneKind::Split)
    }

    pub(crate) fn pane_webview(&self, kind: PaneKind) -> Option<servo::WebView> {
        self.pane(kind).map(|pane| pane.webview.clone())
    }

    pub(crate) fn pane_cursor(&self, kind: PaneKind) -> Option<servo::Cursor> {
        self.pane(kind).map(Pane::cursor)
    }

    pub(crate) fn pane_focus(&self) -> Option<PaneKind> {
        self.pane_focus.get().filter(|kind| self.pane(*kind).is_some())
    }

    /// Give keyboard focus to a pane, or to the active tab (`None`).
    pub(crate) fn set_pane_focus(&self, focus: Option<PaneKind>, window: &ServoShellWindow) {
        self.pane_focus.set(focus);
        let window_focused = window.has_system_focus();
        if let Some(active) = window.active_webview() {
            active.set_focused(focus.is_none() && window_focused);
        }
        for kind in [PaneKind::Split, PaneKind::Glance] {
            if let Some(pane) = self.pane(kind) {
                pane.webview.set_focused(focus == Some(kind) && window_focused);
            }
        }
    }

    /// The window became active or inactive: exactly one of Stillwater's WebViews (the one
    /// with keyboard focus) counts as focused while the window is active, and none while it
    /// isn't, so every page, pane, the sidebar and the command bar paint their selection in
    /// the inactive colour and hide their caret while another app is in front.
    pub(crate) fn apply_window_focus(&self, window: &ServoShellWindow) {
        let window_focused = window.has_system_focus();
        let bar = self.command_bar.webview();
        let sidebar = bar.is_none() && self.sidebar.is_focused();
        let pane = if bar.is_some() || sidebar {
            None
        } else {
            self.pane_focus()
        };
        if let Some(bar) = bar {
            bar.set_focused(window_focused);
        }
        if let Some(webview) = self.sidebar.webview() {
            webview.set_focused(sidebar && window_focused);
        }
        for kind in [PaneKind::Split, PaneKind::Glance] {
            if let Some(pane_view) = self.pane(kind) {
                pane_view.webview.set_focused(pane == Some(kind) && window_focused);
            }
        }
        if let Some(active) = window.active_webview() {
            active.set_focused(bar.is_none() && !sidebar && pane.is_none() && window_focused);
        }
    }

    /// Whether `point` is on the find bar (handled by egui, not the page).
    /// Whether `point` is on the find bar or another egui overlay drawn over the page (the
    /// saved-login list), whose clicks are egui's and not the page's.
    pub(crate) fn find_bar_contains(&self, point: Point2D<f32, DeviceIndependentPixel>) -> bool {
        self.find_bar.contains(pos2(point.x, point.y)) || self.password_chooser_contains(point)
    }

    /// Whether `point` is on the saved-login list under a focused sign-in field.
    pub(crate) fn password_chooser_contains(
        &self,
        point: Point2D<f32, DeviceIndependentPixel>,
    ) -> bool {
        self.password_chooser_rect
            .is_some_and(|rect| rect.contains(pos2(point.x, point.y)))
    }

    /// Whether `point` is on a pane's buttons (handled by egui, not the page).
    pub(crate) fn pane_controls_contain(&self, point: Point2D<f32, DeviceIndependentPixel>) -> bool {
        let point = pos2(point.x, point.y);
        self.pane_control_rects.borrow().iter().any(|rect| rect.contains(point))
    }

    /// Open, close or convert panes requested since the last update.
    fn apply_pane_requests(
        &mut self,
        state: &RunningAppState,
        window: &ServoShellWindow,
        headed_window: &headed_window::HeadedWindow,
    ) {
        if let Some(url) = crate::desktop::panes::take_glance_url() {
            self.pane_requests.borrow_mut().push(PaneRequest::Open(PaneKind::Glance, url));
        }
        // The page context menu's link entries.
        for (action, url) in crate::desktop::dialog::take_link_actions() {
            use crate::desktop::dialog::LinkAction;
            match action {
                LinkAction::Glance => {
                    self.pane_requests.borrow_mut().push(PaneRequest::Open(PaneKind::Glance, url))
                },
                LinkAction::Split => {
                    self.pane_requests.borrow_mut().push(PaneRequest::Open(PaneKind::Split, url))
                },
                LinkAction::BackgroundTab => window.queue_user_interface_command(
                    UserInterfaceCommand::NewBackgroundWebViewWithUrl(url),
                ),
                LinkAction::CopyImage | LinkAction::CopyGif => {
                    if let Some(webview) = window.active_webview() {
                        let kind = if action == LinkAction::CopyGif {
                            crate::desktop::capture::Kind::Gif
                        } else {
                            crate::desktop::capture::Kind::Image
                        };
                        crate::desktop::capture::start(state.servo(), webview.id(), kind, url);
                        window.set_needs_update();
                    }
                },
            }
        }
        for point in crate::desktop::dialog::take_layout_report_requests() {
            if let Some(webview) = window.active_webview() {
                crate::desktop::layout_report::save(&webview, point);
            }
        }
        let requests = std::mem::take(&mut *self.pane_requests.borrow_mut());
        for request in requests {
            match request {
                PaneRequest::Open(kind, url) => {
                    let pane = Pane::new(
                        kind,
                        url,
                        state,
                        &self.window_rendering_context,
                        RepaintEventLoopWaker::new(headed_window.event_loop_proxy(), headed_window.id()),
                        headed_window.theme(),
                        window.platform_window().hidpi_scale_factor(),
                    );
                    match kind {
                        PaneKind::Split => self.split = Some(pane),
                        PaneKind::Glance => self.glance = Some(pane),
                    }
                    self.set_pane_focus(Some(kind), window);
                },
                PaneRequest::Close(kind) => {
                    match kind {
                        PaneKind::Split => self.split = None,
                        PaneKind::Glance => self.glance = None,
                    }
                    if self.pane_focus.get() == Some(kind) {
                        self.set_pane_focus(None, window);
                    }
                },
                PaneRequest::GlanceToTab => {
                    if let Some(url) = self.glance.take().and_then(|pane| pane.url()) {
                        window.queue_user_interface_command(UserInterfaceCommand::NewWebViewWithUrl(url));
                    }
                    self.set_pane_focus(None, window);
                },
                PaneRequest::GlanceToSplit => {
                    if let Some(url) = self.glance.take().and_then(|pane| pane.url()) {
                        self.pane_requests.borrow_mut().push(PaneRequest::Open(PaneKind::Split, url));
                        self.apply_pane_requests(state, window, headed_window);
        {
            // The shield shows the page in the URL bar: the focused pane or the active tab.
            let adblock = state.adblock.borrow();
            let page = self.sidebar.navigated_webview(window);
            let url = page.as_ref().and_then(|webview| webview.url());
            let host = url
                .as_ref()
                .filter(|url| matches!(url.scheme(), "http" | "https"))
                .and_then(|url| url.host_str().map(str::to_owned));
            self.sidebar.set_adblock(serde_json::json!({
                "enabled": adblock.enabled(),
                "ready": adblock.ready(),
                "host": host,
                "site": url.as_ref().is_some_and(|url| adblock.enabled_for(url)),
                "blocked": page.map(|webview| adblock.blocked_count(webview.id())).unwrap_or(0),
            }));
        }
        {
            // The translate button shows the page in the URL bar.
            let mut translation = state.translation.borrow_mut();
            translation.set_proxy(&headed_window.event_loop_proxy());
            let page = self.sidebar.navigated_webview(window).map(|webview| webview.id());
            translation.focused(page);
            self.sidebar.set_translate(translation.to_json(page));
        }
                    }
                },
            }
        }
        // Tell the sidebar about the panes (titles, URLs, which one has focus).
        let summary = |pane: &Pane| (pane.title(), pane.url().map(|url| url.to_string()).unwrap_or_default());
        self.sidebar.set_panes(
            self.pane_focus().and_then(|kind| self.pane_webview(kind)),
            self.split.as_ref().map(summary),
            self.glance.as_ref().map(summary),
        );
    }

    /// Import from Zen profile number `profile`; returns what happened, for the settings panel.
    fn zen_import(
        &mut self,
        state: &RunningAppState,
        profile: usize,
        pins: bool,
        tabs: bool,
        passwords: bool,
        history: bool,
        bookmarks: bool,
        cookies: bool,
    ) -> String {
        let profiles = zen_import::find_profiles();
        let Some(profile) = profiles.get(profile) else {
            return "Zen's profile wasn't found.".into();
        };
        let data = zen_import::read_profile(&profile.path);
        let mut done = Vec::new();
        let mut problems = Vec::new();
        // Things worth knowing that aren't errors.
        let mut notes = Vec::new();
        let plural = |count: usize, one: &str, many: &str| {
            format!("{count} {}", if count == 1 { one } else { many })
        };
        // Only mention what was actually added.
        let mut add = |count: usize, one: &str, many: &str| {
            if count > 0 {
                done.push(plural(count, one, many));
            }
        };
        if pins || tabs {
            match &data.session {
                Ok(session) => {
                    let counts = self.sidebar.import_zen_session(session, pins, tabs);
                    add(counts.spaces, "space", "spaces");
                    add(counts.pins, "pinned tab", "pinned tabs");
                    add(counts.essentials, "Essential", "Essentials");
                    add(counts.folders, "folder", "folders");
                    add(counts.tabs, "tab", "tabs");
                },
                Err(error) => problems.push(error.clone()),
            }
        }
        if passwords {
            match data.logins {
                Ok(logins) => {
                    let mut store = state.passwords.borrow_mut();
                    let total = logins.len();
                    let added = logins
                        .into_iter()
                        .filter(|login| {
                            store.add(
                                login.origin.clone(),
                                login.username.clone(),
                                login.password.clone(),
                                login.created,
                                login.last_used,
                                login.times_used,
                            )
                        })
                        .count();
                    store.commit();
                    add(added, "password", "passwords");
                    if added < total {
                        notes.push(format!("{} already saved.", plural(total - added, "login was", "logins were")));
                    }
                    if data.skipped_logins > 0 {
                        notes.push(format!(
                            "{} skipped (sign-ins for HTTP authentication, or unreadable).",
                            plural(data.skipped_logins, "login", "logins")
                        ));
                    }
                },
                Err(error) => problems.push(error),
            }
        }
        if history || bookmarks {
            match &data.places {
                Ok(places) => {
                    let mut store = state.history.borrow_mut();
                    if bookmarks {
                        add(store.bookmarks.import(&places.bookmarks), "bookmark", "bookmarks");
                    }
                    if history {
                        match store.import(&places.visits) {
                            Ok(added) => add(added, "visit", "visits"),
                            Err(_) => problems.push("Zen's history couldn't be saved.".to_owned()),
                        }
                    }
                },
                Err(error) => problems.push(error.clone()),
            }
        }
        if cookies {
            match &data.cookies {
                Ok(list) => {
                    let count =
                        zen_import::import_cookies(state.servo().site_data_manager(), list);
                    crate::desktop::diagnostics::log(format!(
                        "zen import: {count} cookies handed to the cookie store"
                    ));
                    add(count, "cookie", "cookies");
                    if count > 0 {
                        notes.push(
                            "Some sites also tie a sign-in to things cookies don't carry, so they may still ask you to sign in."
                                .to_owned(),
                        );
                    }
                },
                Err(error) => problems.push(error.clone()),
            }
        }
        let mut result = if !done.is_empty() {
            format!("Imported {}.", done.join(", "))
        } else if problems.is_empty() {
            "Nothing new to import.".to_owned()
        } else {
            String::new()
        };
        for problem in problems.into_iter().chain(notes) {
            if !result.is_empty() {
                result.push(' ');
            }
            result.push_str(&problem);
        }
        result
    }

    pub(crate) fn sidebar_relative_point(
        &self,
        position: Point2D<f32, DeviceIndependentPixel>,
    ) -> Option<Point2D<f32, DeviceIndependentPixel>> {
        let rect = self.sidebar_rect?;
        rect.contains(pos2(position.x, position.y))
            .then(|| Point2D::new(position.x - rect.min.x, position.y - rect.min.y))
    }

    /// Act on what the sidebar page (or a keyboard shortcut) asked for: switch, open and close
    /// tabs, window buttons, panes... Called from `update` and, so that a tab switch starts at
    /// once rather than at the next redraw, as soon as such a command arrives (see
    /// `HeadedWindow::update_user_interface_state`).
    pub(crate) fn handle_sidebar_commands(
        &mut self,
        state: &RunningAppState,
        window: &ServoShellWindow,
        headed_window: &headed_window::HeadedWindow,
    ) {
        self.sidebar.handle_messages(window);

        // Window operations asked for by the sidebar page. Moving and maximising the window
        // are the top bar's alone (`HeadedWindow::handle_top_bar_event`).
        let winit_window = headed_window.winit_window();
        for action in self.sidebar.take_window_actions() {
            crate::desktop::diagnostics::log(format!("window action: {action:?}"));
            match action {
                WindowAction::Minimize => winit_window.set_minimized(true),
                WindowAction::Close => window.schedule_close(),
                WindowAction::ReportSite(tab) => {
                    let webview = tab
                        .and_then(|id| window.webview_by_id(id))
                        .or_else(|| window.active_webview());
                    if let Some(webview) = webview &&
                        let Some(url) = webview.url()
                    {
                        let errors = state.recent_console_errors(webview.id());
                        report::open_externally(&report::issue_url(&url, &errors));
                    }
                },
                WindowAction::OpenPane(kind, url) => self.request_pane(PaneRequest::Open(kind, url)),
                WindowAction::ClosePane(kind) => self.request_pane(PaneRequest::Close(kind)),
                WindowAction::AdblockToggleSite => {
                    if let Some(webview) = self.sidebar.navigated_webview(window) &&
                        let Some(url) = webview.url()
                    {
                        state.adblock.borrow_mut().toggle_site(&url);
                        webview.reload();
                    }
                },
                WindowAction::AdblockSetEnabled(enabled) => {
                    state.adblock.borrow_mut().set_enabled(enabled);
                    if let Some(webview) = self.sidebar.navigated_webview(window) {
                        webview.reload();
                    }
                },
                WindowAction::Password(action) => {
                    let page = self.sidebar.navigated_webview(window);
                    let revealed = state.passwords.borrow_mut().act(action, page.as_ref());
                    if let Some((id, password)) = revealed {
                        self.sidebar.reveal_password(id, &password);
                    }
                },
                WindowAction::Find(command) => self.find_bar.handle(command),
                WindowAction::ZenScan(profile) => self.sidebar.set_zen(zen_scan(profile)),
                WindowAction::ZenImport {
                    profile,
                    pins,
                    tabs,
                    passwords,
                    history,
                    bookmarks,
                    cookies,
                } => {
                    let result = self.zen_import(
                        state, profile, pins, tabs, passwords, history, bookmarks, cookies,
                    );
                    let mut zen = zen_scan(profile);
                    zen["result"] = serde_json::Value::String(result);
                    self.sidebar.set_zen(zen);
                },
                // Tests: run JavaScript in the floating new-tab address bar.
                WindowAction::History(command, argument) if command == "hist-bar-eval" => {
                    self.command_bar.debug_eval(&argument)
                },
                WindowAction::History(command, argument) => crate::desktop::history::on_command(
                    state,
                    window,
                    &mut self.sidebar,
                    &command,
                    &argument,
                ),
                WindowAction::CommandBar => self.command_bar.request_toggle(),
                WindowAction::Download(command, argument) => {
                    let page = self.sidebar.navigated_webview(window).map(|webview| webview.id());
                    state
                        .downloads
                        .borrow_mut()
                        .command(&command, &argument, state.servo(), page);
                },
                WindowAction::Compare => {
                    if !self.sidebar.comparing() &&
                        let Some(webview) = self.sidebar.navigated_webview(window)
                    {
                        self.sidebar.set_compare(serde_json::json!({ "busy": true, "message": "Starting…" }));
                        crate::desktop::compare::start(
                            &webview,
                            headed_window.theme(),
                            headed_window.event_loop_proxy(),
                        );
                    }
                },
                WindowAction::Translate(command, argument) => {
                    let page = self.sidebar.navigated_webview(window);
                    state
                        .translation
                        .borrow_mut()
                        .command(&command, &argument, page);
                },
                WindowAction::CheckForUpdates => {
                    updater::check_now(headed_window.event_loop_proxy());
                },
                WindowAction::Update => {
                    let Some(update) = self.sidebar.update().cloned() else {
                        continue;
                    };
                    let proxy = headed_window.event_loop_proxy();
                    if update.installable {
                        if !self.sidebar.update_in_progress() {
                            self.sidebar.set_update_status(UpdateStatus::Downloading);
                            updater::install(update, proxy);
                        }
                    } else {
                        // Portable copies can't replace themselves: show the release page.
                        let _ = proxy.send_event(AppEvent::OpenUrl(update.page_url, None));
                    }
                },
            }
        }
        self.apply_pane_requests(state, window, headed_window);
    }

    /// Update the user interface, but do not paint the updated state.
    pub(crate) fn update(
        &mut self,
        state: &RunningAppState,
        window: &ServoShellWindow,
        headed_window: &headed_window::HeadedWindow,
    ) {
        headed_window.set_theme_override(self.sidebar.theme_override(), window);
        let theme = headed_window.theme();
        state.sync_color_scheme_hint(theme);
        let hidpi_scale = window.platform_window().hidpi_scale_factor();
        self.sidebar.ensure_webview(state, theme, hidpi_scale);
        load_pending_favicons(window, &mut self.sidebar);
        self.handle_sidebar_commands(state, window, headed_window);
        let winit_window = headed_window.winit_window();
        {
            // The shield shows the page in the URL bar: the focused pane or the active tab.
            let adblock = state.adblock.borrow();
            let page = self.sidebar.navigated_webview(window);
            let url = page.as_ref().and_then(|webview| webview.url());
            let host = url
                .as_ref()
                .filter(|url| matches!(url.scheme(), "http" | "https"))
                .and_then(|url| url.host_str().map(str::to_owned));
            self.sidebar.set_adblock(serde_json::json!({
                "enabled": adblock.enabled(),
                "ready": adblock.ready(),
                "host": host,
                "site": url.as_ref().is_some_and(|url| adblock.enabled_for(url)),
                "blocked": page.map(|webview| adblock.blocked_count(webview.id())).unwrap_or(0),
            }));
        }
        {
            // The translate button shows the page in the URL bar.
            let mut translation = state.translation.borrow_mut();
            translation.set_proxy(&headed_window.event_loop_proxy());
            let page = self.sidebar.navigated_webview(window).map(|webview| webview.id());
            translation.focused(page);
            self.sidebar.set_translate(translation.to_json(page));
        }
        {
            let page = self.sidebar.navigated_webview(window);
            let url = page.as_ref().and_then(|webview| webview.url());
            self.sidebar
                .set_passwords(state.passwords.borrow().to_json(url.as_ref()));
            self.sidebar
                .set_hist(state.history.borrow().sidebar_json(url.as_ref()));
        }
        self.command_bar.update(
            state,
            window,
            &mut self.sidebar,
            &self.window_rendering_context,
            || RepaintEventLoopWaker::new(headed_window.event_loop_proxy(), headed_window.id()),
            theme,
        );
        {
            let mut downloads = state.downloads.borrow_mut();
            downloads.set_event_loop_proxy(headed_window.event_loop_proxy());
            self.sidebar.set_downloads(downloads.to_json());
        }
        self.sidebar.set_playing_media(state.playing_media());
        self.sidebar
            .set_window_state(!winit_window.is_decorated(), winit_window.is_maximized());
        self.sidebar.push_state(window);

        if !self.splash_decided {
            self.splash_decided = true;
            let automated = state.servoshell_preferences.webdriver_port.get().is_some();
            if !crate::desktop::splash::Splash::wanted(self.sidebar.startup_animation(), automated) {
                self.splash = None;
            }
        }

        self.rendering_context
            .make_current()
            .expect("Could not make RenderingContext current");
        let Self {
            rendering_context,
            context,
            sidebar,
            sidebar_rect,
            content_rect,
            painted_webviews,
            top_bar_rect,
            top_bar_zone,
            split,
            glance,
            split_rect,
            glance_rect,
            pane_requests,
            pane_control_rects,
            command_bar,
            pane_focus,
            find_bar,
            password_chooser_rect,
            splash,
            scrollbars,
            ..
        } = self;
        pane_control_rects.borrow_mut().clear();
        let mut picked_login = None;

        let winit_window = headed_window.winit_window();
        context.run(winit_window, |ctx| {
            let scale =
                Scale::<_, DeviceIndependentPixel, DevicePixel>::new(ctx.pixels_per_point());

            headed_window.for_each_active_dialog(window, |dialog| dialog.update(ctx));

            // TODO: While in fullscreen add some way to mitigate the increased phishing risk
            // when not displaying the URL bar: https://github.com/servo/servo/issues/32443
            let full_rect = ctx.available_rect_before_wrap();
            let (new_sidebar_rect, content_area) =
                sidebar.layout(full_rect, headed_window.is_fullscreen_from_document());
            *sidebar_rect = new_sidebar_rect;
            // Split view: the active tab on the left, the split pane on the right.
            let (new_content_rect, new_split_rect) = match split {
                Some(_) => {
                    let gap = crate::desktop::sidebar::CONTENT_MARGIN;
                    let half = ((content_area.width() - gap) / 2.0).floor();
                    (
                        egui::Rect::from_min_size(content_area.min, egui::vec2(half, content_area.height())),
                        Some(egui::Rect::from_min_max(
                            pos2(content_area.min.x + half + gap, content_area.min.y),
                            content_area.max,
                        )),
                    )
                },
                None => (content_area, None),
            };
            *content_rect = new_content_rect;
            *split_rect = new_split_rect;

            let [r, g, b] = sidebar.background();
            let background = Color32::from_rgb(r, g, b);
            ctx.layer_painter(LayerId::background())
                .rect_filled(full_rect, 0.0, background);

            // Build a graft node for each WebView.
            let affine = {
                // The grafted WebView tree reports bounds in device pixels relative to the
                // WebView's own origin, so this node supplies the offset of the WebView within
                // the window, and scales it to the same scale as the rest of the nodes in egui's
                // AccessKit tree.
                let scale = (1.0 / window.platform_window().hidpi_scale_factor().get()) as f64;
                let x = new_content_rect.min.x as f64;
                let y = new_content_rect.min.y as f64;
                Affine::new([scale, 0.0, 0.0, scale, x, y])
            };
            for (webview_id, webview) in window.webviews() {
                if let Some(tree_id) = webview.accesskit_tree_id() {
                    let id = egui::Id::new(webview_id);
                    ctx.accesskit_node_builder(id, |node| {
                        node.set_tree_id(tree_id);
                        // Only the transform is set: AccessKit consumers exclude graft nodes from
                        // the presented tree, so bounds on this node would never be read.
                        node.set_transform(affine);
                    });
                }
            }

            // Stillwater: a new scale factor (the window moved to a monitor with another DPI)
            // reaches the tabs right before the size change it comes with, so that the messages
            // are adjacent and the page lays out once, at the new size and scale.
            if window.active_webview().is_some_and(|webview| {
                webview.hidpi_scale_factor() != window.platform_window().hidpi_scale_factor()
            }) {
                window.hidpi_scale_factor_changed();
            }

            // If the layout changed size, then update the size of the WebView and also
            // the size of its RenderingContext.
            let size = Size2D::new(new_content_rect.width(), new_content_rect.height()) * scale;
            if let Some(webview) = window.active_webview() &&
                size != webview.size()
            {
                // `rect` is sized to just the WebView viewport, which is required by
                // `OffscreenRenderingContext` See:
                // <https://github.com/servo/servo/issues/38369#issuecomment-3138378527>
                webview.resize(PhysicalSize::new(size.width as u32, size.height as u32))
            }

            if let Some(status_text) = &self.status_text {
                egui::Tooltip::always_open(
                    ctx.clone(),
                    LayerId::new(Order::Tooltip, Id::new("tooltip")),
                    "tooltip layer".into(),
                    pos2(new_content_rect.min.x, new_content_rect.max.y),
                )
                .show(|ui| ui.add(Label::new(status_text.clone()).extend()));
            }

            window.repaint_webviews();

            // The split pane, beside the active tab.
            if let (Some(pane), Some(rect)) = (split.as_ref(), new_split_rect) {
                pane.paint(rect, scale);
                if pane.webview.load_status() != LoadStatus::Started {
                    add_surface(ctx, LayerId::background(), rect, pane.rendering_context());
                }
                // Its close button shows while the pointer is near the pane's top edge.
                let pointer = ctx.input(|input| input.pointer.hover_pos());
                let near_top = pointer.is_some_and(|p| {
                    egui::Rect::from_min_size(rect.min, egui::vec2(rect.width(), 48.0)).contains(p)
                });
                let at = pos2(rect.max.x - 40.0, rect.min.y + 8.0);
                let controls = egui::Rect::from_min_size(at, egui::vec2(32.0, 32.0)).expand(4.0);
                if near_top || pointer.is_some_and(|p| controls.contains(p)) {
                    pane_control_rects.borrow_mut().push(controls);
                    if pane_controls(ctx, "split-controls", at, &[PaneButton::Close], theme)
                        == Some(PaneButton::Close)
                    {
                        pane_requests.borrow_mut().push(PaneRequest::Close(PaneKind::Split));
                    }
                }
            }
            // A page's render surface is white until the page first draws; show the window
            // background instead of a white flash until then (once per tab): from the page's
            // first contentful frame, not only once it has fully loaded.
            let active = window.active_webview();
            // (The page was just rendered above, so a first contentful frame shows at once.)
            if let Some(webview) = &active &&
                !painted_webviews.contains(&webview.id())
            {
                let complete = webview.load_status() == LoadStatus::Complete;
                if complete || webview.has_contentful_paint() {
                    painted_webviews.insert(webview.id());
                    if !complete {
                        crate::desktop::diagnostics::log(format!(
                            "tab shows its page from its first contentful frame: {}",
                            webview.url().map(|url| url.to_string()).unwrap_or_default()
                        ));
                    }
                }
            }
            painted_webviews.retain(|id| window.webview_by_id(*id).is_some());
            let content_ready = active.is_some_and(|webview| painted_webviews.contains(&webview.id()));

            if content_ready && let Some(render_to_parent) = rendering_context.render_to_parent_callback() {
                ctx.layer_painter(LayerId::background()).add(PaintCallback {
                    rect: new_content_rect,
                    callback: Arc::new(CallbackFn::new(move |info, painter| {
                        let clip = info.viewport_in_pixels();
                        let rect_in_parent = Rect::new(
                            Point2D::new(clip.left_px, clip.from_bottom_px),
                            Size2D::new(clip.width_px, clip.height_px),
                        );
                        render_to_parent(painter.gl(), rect_in_parent)
                    })),
                });
            }

            // Round the corners of the web content like Zen does, by painting a thick
            // background-coloured stroke whose inner edge is the rounded content rect.
            if content_area != full_rect {
                let thickness = CONTENT_RADIUS * 2.0;
                let gap = crate::desktop::sidebar::CONTENT_MARGIN;
                let painter = ctx.layer_painter(LayerId::new(Order::Middle, Id::new("content-corners")));
                for rect in [Some(new_content_rect), new_split_rect].into_iter().flatten() {
                    painter.with_clip_rect(rect.expand(gap / 2.0)).rect_stroke(
                        rect.expand(thickness / 2.0),
                        CornerRadius::same((CONTENT_RADIUS + thickness / 2.0) as u8),
                        Stroke::new(thickness, background),
                        StrokeKind::Middle,
                    );
                }
            }

            // Glance: a link preview floating over the dimmed page, with its controls beside it.
            *glance_rect = None;
            if let Some(pane) = glance.as_ref() {
                let rect = egui::Rect::from_center_size(
                    content_area.center(),
                    egui::vec2(
                        (content_area.width() * 0.8).floor(),
                        (content_area.height() * 0.9).floor(),
                    ),
                );
                ctx.layer_painter(LayerId::new(Order::Middle, Id::new("glance-backdrop")))
                    .rect_filled(
                        content_area,
                        CornerRadius::same(CONTENT_RADIUS as u8),
                        Color32::from_black_alpha(120),
                    );
                pane.paint(rect, scale);
                let layer = LayerId::new(Order::Foreground, Id::new("glance"));
                if pane.webview.load_status() != LoadStatus::Started {
                    add_surface(ctx, layer, rect, pane.rendering_context());
                } else {
                    ctx.layer_painter(layer).rect_filled(rect, CornerRadius::ZERO, background);
                }
                let at = pos2(rect.max.x + 10.0, rect.min.y);
                let buttons = [PaneButton::Close, PaneButton::OpenAsTab, PaneButton::Split];
                pane_control_rects.borrow_mut().push(egui::Rect::from_min_size(
                    at,
                    egui::vec2(32.0, buttons.len() as f32 * 40.0),
                ).expand(4.0));
                match pane_controls(ctx, "glance-controls", at, &buttons, theme) {
                    Some(PaneButton::Close) => {
                        pane_requests.borrow_mut().push(PaneRequest::Close(PaneKind::Glance))
                    },
                    Some(PaneButton::OpenAsTab) => pane_requests.borrow_mut().push(PaneRequest::GlanceToTab),
                    Some(PaneButton::Split) => pane_requests.borrow_mut().push(PaneRequest::GlanceToSplit),
                    None => {},
                }
                *glance_rect = Some(rect);
            }

            // The page scrollbars, over the pages that take input: only the Glance while it's
            // open, none while the floating address bar is.
            {
                let mut views = Vec::new();
                if command_bar.webview().is_none() {
                    if let (Some(pane), Some(rect)) = (glance.as_ref(), *glance_rect) {
                        if pane.webview.load_status() != LoadStatus::Started {
                            views.push((
                                pane.webview.clone(),
                                rect,
                                LayerId::new(Order::Foreground, Id::new("glance")),
                            ));
                        }
                    } else {
                        let layer = LayerId::new(Order::Middle, Id::new("scrollbars"));
                        if content_ready && let Some(webview) = window.active_webview() {
                            views.push((webview, new_content_rect, layer));
                        }
                        if let (Some(pane), Some(rect)) = (split.as_ref(), new_split_rect) &&
                            pane.webview.load_status() != LoadStatus::Started
                        {
                            views.push((pane.webview.clone(), rect, layer));
                        }
                    }
                }
                scrollbars.get_mut().paint(ctx, &views);
            }

            command_bar.paint(ctx, content_area, scale, theme);
            // The find bar, over the page it searches.
            {
                let (webview, rect) = match pane_focus.get() {
                    Some(PaneKind::Glance) if glance_rect.is_some() => {
                        (glance.as_ref().map(|pane| pane.webview.clone()), *glance_rect)
                    },
                    Some(PaneKind::Split) if split.is_some() => {
                        (split.as_ref().map(|pane| pane.webview.clone()), new_split_rect)
                    },
                    _ => (window.active_webview(), Some(new_content_rect)),
                };
                find_bar.update(
                    ctx,
                    window,
                    webview,
                    rect.unwrap_or(new_content_rect),
                    theme,
                );
            }

            // The saved logins under a focused sign-in field in the active tab.
            *password_chooser_rect = None;
            if let Some(webview) = window.active_webview() &&
                glance.is_none()
            {
                let passwords = state.passwords.borrow();
                if let Some(chooser) = passwords.chooser_for(&webview) {
                    let zoom = webview.page_zoom();
                    let (rect, picked) = crate::desktop::passwords::draw_chooser(
                        ctx,
                        chooser,
                        new_content_rect,
                        zoom,
                        theme,
                    );
                    *password_chooser_rect = Some(rect);
                    picked_login = picked.map(|index| (webview.clone(), index));
                }
            }

            // Zen's top bar: the window buttons slide in when the mouse touches the top edge
            // and hide again when it leaves. Its empty part drags the window.
            {
                let winit_window = headed_window.winit_window();
                let frameless = !winit_window.is_decorated() && !headed_window.is_fullscreen_from_document();
                let bar = egui::Rect::from_min_max(
                    pos2(content_area.min.x, full_rect.min.y),
                    pos2(full_rect.max.x, full_rect.min.y + TOP_BAR_HEIGHT),
                );
                let pointer = ctx.input(|input| input.pointer.hover_pos());
                let reveal = pointer.is_some_and(|p| p.y <= full_rect.min.y + TOP_BAR_REVEAL && p.x >= bar.min.x);
                let keep = top_bar_rect.is_some() && pointer.is_some_and(|p| bar.expand(4.0).contains(p));
                // STILLWATER_SHOW_TOP_BAR keeps it visible (for screenshots in tests).
                let forced = std::env::var_os("STILLWATER_SHOW_TOP_BAR").is_some();
                *top_bar_rect = (frameless && (reveal || keep || forced)).then_some(bar);
                *top_bar_zone = frameless.then_some(bar);
            }
            if let Some(bar) = *top_bar_rect {
                draw_top_bar(ctx, bar, background, theme, headed_window);
            }

            // Paint the sidebar's surface every frame, even while it's collapsed in compact
            // mode: an unconsumed frame stalls Servo's renderer and freezes the window.
            let sidebar_paint_rect = new_sidebar_rect.unwrap_or_else(|| {
                egui::Rect::from_min_size(
                    full_rect.min,
                    egui::vec2(sidebar.width_for(full_rect.width()), full_rect.height()),
                )
            });
            sidebar.paint(sidebar_paint_rect, scale);
            if let Some(sidebar_rect) = new_sidebar_rect {
                // Likewise, wait for the sidebar page to load before showing its surface.
                if sidebar.is_ready() && let Some(render_to_parent) =
                    sidebar.rendering_context().render_to_parent_callback()
                {
                    ctx.layer_painter(LayerId::new(Order::Foreground, Id::new("sidebar")))
                        .add(PaintCallback {
                            rect: sidebar_rect,
                            callback: Arc::new(CallbackFn::new(move |info, painter| {
                                let clip = info.viewport_in_pixels();
                                let rect_in_parent = Rect::new(
                                    Point2D::new(clip.left_px, clip.from_bottom_px),
                                    Size2D::new(clip.width_px, clip.height_px),
                                );
                                render_to_parent(painter.gl(), rect_in_parent)
                            })),
                        });
                }
            }

            // The startup animation covers everything until the browser is ready to be seen.
            if let Some(animation) = splash.as_mut() &&
                !animation.paint(ctx, full_rect, background, sidebar.is_ready())
            {
                *splash = None;
            }
        });

        if let Some((webview, index)) = picked_login {
            state.passwords.borrow_mut().pick(&webview, index);
            window.set_needs_update();
        }

        // If any egui widget requested a repaint, also request a repaint for our
        // containing window. This allows egui widget to animate on their own.
        if self.context.egui_ctx.has_requested_repaint() {
            window.set_needs_repaint();
        }

        let adapter = self
            .context
            .egui_winit
            .accesskit
            .as_mut()
            .expect("guaranteed by Gui::new()");
        for tree_update in self.pending_accesskit_updates.drain(..) {
            adapter.update_if_active(|| tree_update);
        }
    }

    /// Paint the GUI, as of the last update.
    pub(crate) fn paint(&mut self, window: &Window) {
        self.rendering_context
            .make_current()
            .expect("Could not make RenderingContext current");
        self.rendering_context
            .parent_context()
            .prepare_for_rendering();
        self.context.paint(window);
        self.rendering_context.parent_context().present();
        if !self.shown {
            self.shown = true;
            window.set_visible(true);
            crate::desktop::diagnostics::log("first frame painted; window shown");
        }
    }

    fn update_status_text(&mut self, window: &ServoShellWindow) -> bool {
        let state_status = window
            .active_webview()
            .and_then(|webview| webview.status_text())
            .or_else(crate::desktop::capture::notice);
        let old_status = std::mem::replace(&mut self.status_text, state_status);
        old_status != self.status_text
    }

    /// Updates all fields taken from the given [`ServoShellWindow`] and pushes fresh state to
    /// the sidebar page. Returns true iff the egui needs an update.
    pub(crate) fn update_webview_data(&mut self, window: &ServoShellWindow) -> bool {
        self.sidebar.push_state(window);
        self.update_status_text(window)
    }

    /// Returns true if a redraw is required after handling the provided event.
    pub(crate) fn handle_accesskit_event(
        &mut self,
        event: &egui_winit::accesskit_winit::WindowEvent,
    ) -> bool {
        match event {
            egui_winit::accesskit_winit::WindowEvent::InitialTreeRequested => {
                self.context.egui_ctx.enable_accesskit();
                true
            },
            egui_winit::accesskit_winit::WindowEvent::ActionRequested(req) => {
                self.context
                    .egui_winit
                    .on_accesskit_action_request(req.clone());
                true
            },
            egui_winit::accesskit_winit::WindowEvent::AccessibilityDeactivated => {
                self.context.egui_ctx.disable_accesskit();
                false
            },
        }
    }

    pub(crate) fn set_zoom_factor(&self, factor: f32) {
        self.context.egui_ctx.set_zoom_factor(factor);
    }

    pub(crate) fn notify_accessibility_tree_update(&mut self, tree_update: accesskit::TreeUpdate) {
        self.pending_accesskit_updates.push(tree_update);
    }
}

fn embedder_image_to_egui_image(image: &Image) -> egui::ColorImage {
    let width = image.width as usize;
    let height = image.height as usize;

    match image.format {
        PixelFormat::K8 => egui::ColorImage::from_gray([width, height], image.data()),
        PixelFormat::KA8 => {
            // Convert to rgba
            let (chunks, _) = image.data().as_chunks::<2>();
            let data: Vec<u8> = chunks
                .iter()
                .flat_map(|pixel| [pixel[0], pixel[0], pixel[0], pixel[1]])
                .collect();
            egui::ColorImage::from_rgba_unmultiplied([width, height], &data)
        },
        PixelFormat::RGB8 => egui::ColorImage::from_rgb([width, height], image.data()),
        PixelFormat::RGBA8 => {
            egui::ColorImage::from_rgba_unmultiplied([width, height], image.data())
        },
        PixelFormat::BGRA8 => {
            // Convert from BGRA to RGBA
            let (chunks, _) = image.data().as_chunks::<4>();
            let data: Vec<u8> = chunks
                .iter()
                .flat_map(|chunk| [chunk[2], chunk[1], chunk[0], chunk[3]])
                .collect();
            egui::ColorImage::from_rgba_unmultiplied([width, height], &data)
        },
    }
}


/// The Zen profiles found and what profile number `profile` holds, for the import section in
/// settings.
fn zen_scan(profile: usize) -> serde_json::Value {
    let profiles = zen_import::find_profiles();
    let names: Vec<&str> = profiles.iter().map(|profile| profile.name.as_str()).collect();
    let Some(chosen) = profiles.get(profile).or(profiles.first()) else {
        return serde_json::json!({ "profiles": names });
    };
    let data = zen_import::read_profile(&chosen.path);
    let session = data.session.as_ref().ok();
    let count = |test: fn(&zen_import::ZenTab) -> bool| {
        session.map_or(0, |session| session.tabs.iter().filter(|tab| test(tab)).count())
    };
    serde_json::json!({
        "profiles": names,
        "profile": profile.min(names.len() - 1),
        "spaces": session.map_or(0, |session| session.spaces.len()),
        "pins": count(|tab| tab.pinned && !tab.essential),
        "essentials": count(|tab| tab.essential),
        "tabs": count(|tab| !tab.pinned && !tab.essential),
        "passwords": data.logins.as_ref().map_or(0, Vec::len),
        "sessionProblem": data.session.as_ref().err(),
        "passwordProblem": data.logins.as_ref().err(),
        "bookmarks": data.places.as_ref().map_or(0, |places| places.bookmarks.len()),
        "visits": data.places.as_ref().map_or(0, |places| places.visits.len()),
        "placesProblem": data.places.as_ref().err(),
        "cookies": data.cookies.as_ref().map_or(0, Vec::len),
        "cookiesProblem": data.cookies.as_ref().err(),
    })
}

/// Largest favicon kept for the sidebar, in pixels a side (20 CSS px at up to 240% scaling).
const FAVICON_MAX: u32 = 48;

/// Converts all favicons that have not yet been processed into PNG data URLs for the sidebar.
fn load_pending_favicons(window: &ServoShellWindow, sidebar: &mut Sidebar) {
    for id in window.take_pending_favicon_loads() {
        let Some(webview) = window.webview_by_id(id) else {
            continue;
        };
        let Some(favicon) = webview.favicon() else {
            continue;
        };

        let image = embedder_image_to_egui_image(&favicon);
        let rgba: Vec<u8> = image
            .pixels
            .iter()
            .flat_map(|pixel| pixel.to_srgba_unmultiplied())
            .collect();
        let (width, height) = (image.size[0] as u32, image.size[1] as u32);
        // The sidebar shows icons at 16-20 CSS px. Sites often offer 64-512 px icons, and the
        // data URL goes into every state push, every sidebar render and the session file
        // (which drops icons over 16 KB): keep at most FAVICON_MAX px a side.
        let (width, height, rgba) = match image::RgbaImage::from_raw(width, height, rgba) {
            Some(full) if width.max(height) > FAVICON_MAX => {
                let scale = FAVICON_MAX as f32 / width.max(height) as f32;
                let fit = |side: u32| ((side as f32 * scale).round() as u32).max(1);
                let small = image::imageops::thumbnail(&full, fit(width), fit(height));
                (small.width(), small.height(), small.into_raw())
            },
            Some(full) => (width, height, full.into_raw()),
            None => continue,
        };
        if let Some(data_url) = png_data_url(width, height, rgba) {
            sidebar.set_favicon(id, data_url);
        }
    }
}

/// Height of Zen's hover-reveal top bar.
const TOP_BAR_HEIGHT: f32 = 36.0;
/// The top bar slides in when the pointer is this close to the window's top edge.
const TOP_BAR_REVEAL: f32 = 8.0;
const CAPTION_BUTTON_WIDTH: f32 = 46.0;

/// What a press on the top bar does. The press is handled by `HeadedWindow` (never by egui or
/// the page): an OS window drag swallows the release, which used to leave egui thinking the
/// button was still down, so it stopped claiming later presses and they reached the page.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum TopBarHit {
    Drag,
    Close,
    Maximize,
    Minimize,
}

/// The caption button `index` places from the right (close, maximise, minimise).
fn caption_button_rect(bar: egui::Rect, index: usize) -> egui::Rect {
    egui::Rect::from_min_size(
        pos2(bar.max.x - CAPTION_BUTTON_WIDTH * (index as f32 + 1.0), bar.min.y),
        egui::vec2(CAPTION_BUTTON_WIDTH, bar.height()),
    )
}

/// Draw the top bar's background and Windows-style caption buttons (clicks on it are handled
/// by `HeadedWindow::handle_top_bar_event`).
fn draw_top_bar(
    ctx: &egui::Context,
    bar: egui::Rect,
    background: Color32,
    theme: servo::Theme,
    headed_window: &headed_window::HeadedWindow,
) {
    let foreground = match theme {
        servo::Theme::Dark => Color32::from_rgba_unmultiplied(255, 255, 255, 210),
        servo::Theme::Light => Color32::from_rgba_unmultiplied(0, 0, 0, 200),
    };
    let hover = match theme {
        servo::Theme::Dark => Color32::from_rgba_unmultiplied(255, 255, 255, 22),
        servo::Theme::Light => Color32::from_rgba_unmultiplied(0, 0, 0, 18),
    };
    let winit_window = headed_window.winit_window();
    egui::Area::new(Id::new("top-bar"))
        .order(Order::Foreground)
        .fixed_pos(bar.min)
        .show(ctx, |ui| {
            let painter = ui.painter().clone();
            painter.rect_filled(bar, CornerRadius::ZERO, background);

            // Claim the bar's area so egui reports the pointer as over it.
            let _ = ui.interact(bar, Id::new("top-bar-drag"), egui::Sense::hover());

            let buttons = ["close", "maximize", "minimize"];
            for (index, name) in buttons.iter().enumerate() {
                let rect = caption_button_rect(bar, index);
                let response = ui.interact(rect, Id::new(("caption", *name)), egui::Sense::hover());
                let is_close = *name == "close";
                if response.hovered() {
                    let fill = if is_close { Color32::from_rgb(196, 43, 28) } else { hover };
                    painter.rect_filled(rect, CornerRadius::ZERO, fill);
                }
                let color = if is_close && response.hovered() { Color32::WHITE } else { foreground };
                let stroke = Stroke::new(1.0_f32, color);
                let c = rect.center();
                match *name {
                    "minimize" => {
                        painter.line_segment([pos2(c.x - 5.0, c.y), pos2(c.x + 5.0, c.y)], stroke);
                    },
                    "maximize" if winit_window.is_maximized() => {
                        // Restore: two overlapping squares.
                        painter.rect_stroke(
                            egui::Rect::from_min_max(pos2(c.x - 5.0, c.y - 3.0), pos2(c.x + 3.0, c.y + 5.0)),
                            CornerRadius::ZERO,
                            stroke,
                            StrokeKind::Middle,
                        );
                        painter.line_segment([pos2(c.x - 3.0, c.y - 5.0), pos2(c.x + 5.0, c.y - 5.0)], stroke);
                        painter.line_segment([pos2(c.x + 5.0, c.y - 5.0), pos2(c.x + 5.0, c.y + 3.0)], stroke);
                    },
                    "maximize" => {
                        painter.rect_stroke(
                            egui::Rect::from_center_size(c, egui::vec2(10.0, 10.0)),
                            CornerRadius::ZERO,
                            stroke,
                            StrokeKind::Middle,
                        );
                    },
                    _ => {
                        painter.line_segment([pos2(c.x - 5.0, c.y - 5.0), pos2(c.x + 5.0, c.y + 5.0)], stroke);
                        painter.line_segment([pos2(c.x - 5.0, c.y + 5.0), pos2(c.x + 5.0, c.y - 5.0)], stroke);
                    },
                }
            }
        });
}

/// Composite an offscreen rendering context's latest frame into `rect` on `layer`.
fn add_surface(
    ctx: &egui::Context,
    layer: LayerId,
    rect: egui::Rect,
    rendering_context: &Rc<OffscreenRenderingContext>,
) {
    let Some(render_to_parent) = rendering_context.render_to_parent_callback() else {
        return;
    };
    ctx.layer_painter(layer).add(PaintCallback {
        rect,
        callback: Arc::new(CallbackFn::new(move |info, painter| {
            let clip = info.viewport_in_pixels();
            let rect_in_parent = Rect::new(
                Point2D::new(clip.left_px, clip.from_bottom_px),
                Size2D::new(clip.width_px, clip.height_px),
            );
            render_to_parent(painter.gl(), rect_in_parent)
        })),
    });
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum PaneButton {
    Close,
    OpenAsTab,
    Split,
}

/// A column of round icon buttons for a pane, starting at `at`. Returns the one clicked.
fn pane_controls(
    ctx: &egui::Context,
    id: &str,
    at: egui::Pos2,
    buttons: &[PaneButton],
    theme: servo::Theme,
) -> Option<PaneButton> {
    let (fill, hover, foreground) = match theme {
        servo::Theme::Dark => (
            Color32::from_rgb(43, 42, 51),
            Color32::from_rgb(62, 61, 72),
            Color32::from_rgb(235, 235, 240),
        ),
        servo::Theme::Light => (
            Color32::from_rgb(250, 250, 252),
            Color32::from_rgb(232, 232, 238),
            Color32::from_rgb(30, 30, 36),
        ),
    };
    let size = 32.0;
    let mut clicked = None;
    egui::Area::new(Id::new(id))
        .order(Order::Tooltip)
        .fixed_pos(at)
        .show(ctx, |ui| {
            let painter = ui.painter().clone();
            for (index, button) in buttons.iter().enumerate() {
                let rect = egui::Rect::from_min_size(
                    pos2(at.x, at.y + index as f32 * (size + 8.0)),
                    egui::vec2(size, size),
                );
                let response = ui
                    .interact(rect, Id::new((id, index)), egui::Sense::click())
                    .on_hover_text(match button {
                        PaneButton::Close => "Close",
                        PaneButton::OpenAsTab => "Open in new tab",
                        PaneButton::Split => "Open in split view",
                    });
                painter.circle_filled(
                    rect.center(),
                    size / 2.0,
                    if response.hovered() { hover } else { fill },
                );
                painter.circle_stroke(
                    rect.center(),
                    size / 2.0,
                    Stroke::new(1.0_f32, Color32::from_black_alpha(40)),
                );
                let stroke = Stroke::new(1.5_f32, foreground);
                let c = rect.center();
                match button {
                    PaneButton::Close => {
                        painter.line_segment([pos2(c.x - 5.0, c.y - 5.0), pos2(c.x + 5.0, c.y + 5.0)], stroke);
                        painter.line_segment([pos2(c.x - 5.0, c.y + 5.0), pos2(c.x + 5.0, c.y - 5.0)], stroke);
                    },
                    PaneButton::OpenAsTab => {
                        // A box with an arrow leaving its top-right corner.
                        painter.rect_stroke(
                            egui::Rect::from_min_max(pos2(c.x - 6.0, c.y - 4.0), pos2(c.x + 4.0, c.y + 6.0)),
                            CornerRadius::same(2),
                            stroke,
                            StrokeKind::Middle,
                        );
                        painter.line_segment([pos2(c.x - 1.0, c.y + 1.0), pos2(c.x + 7.0, c.y - 7.0)], stroke);
                        painter.line_segment([pos2(c.x + 2.0, c.y - 7.0), pos2(c.x + 7.0, c.y - 7.0)], stroke);
                        painter.line_segment([pos2(c.x + 7.0, c.y - 7.0), pos2(c.x + 7.0, c.y - 2.0)], stroke);
                    },
                    PaneButton::Split => {
                        // Two panes side by side.
                        painter.rect_stroke(
                            egui::Rect::from_center_size(c, egui::vec2(14.0, 11.0)),
                            CornerRadius::same(2),
                            stroke,
                            StrokeKind::Middle,
                        );
                        painter.line_segment([pos2(c.x, c.y - 5.5), pos2(c.x, c.y + 5.5)], stroke);
                    },
                }
                if response.clicked() {
                    clicked = Some(*button);
                }
            }
        });
    clicked
}

/// Whether the (primary) mouse button is physically held down right now.
pub(crate) fn left_button_held() -> bool {
    #[cfg(target_os = "windows")]
    {
        use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON, VK_RBUTTON};
        use windows_sys::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_SWAPBUTTON};
        // Safety: plain Win32 queries with no pointers.
        unsafe {
            let primary = if GetSystemMetrics(SM_SWAPBUTTON) != 0 { VK_RBUTTON } else { VK_LBUTTON };
            return (GetAsyncKeyState(primary as i32) as u16 & 0x8000) != 0;
        }
    }
    #[allow(unreachable_code)]
    true
}
