/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Zen's floating address bar for a new tab (Zen's `zen.urlbar.replace-newtab`): Ctrl+T and
//! the New Tab button don't create a tab. Instead an address bar floats over the dimmed page,
//! with suggestions under it, and only picking something opens a tab. Escape, clicking
//! elsewhere or Ctrl+T again closes it without creating a tab. It also opens by itself over an
//! empty New Tab page (the last tab of a space closed), and then loads into that tab.
//!
//! The sidebar is a 240 px `WebView` that can't draw over the page, so the bar is its own small
//! `WebView` (`servo:sidebar/commandbar.html`) with its own offscreen rendering context, like
//! the Glance pane. Servo can't render a page with a transparent background, so the `WebView`
//! is exactly as big as the bar and its suggestions (the page reports its height) and egui
//! draws the dimmed backdrop and the rounded card around it. The `WebView` is created the first
//! time the bar opens and then kept (hidden while closed), so it opens instantly afterwards.
//!
//! Page -> Rust: `console.log("__bar__<command>\t<argument>")`; Rust -> page: `window.bar.*`.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::Arc;
use std::task::Waker;
use std::time::{Duration, Instant};

use dpi::PhysicalSize;
use egui::{Color32, CornerRadius, Id, LayerId, Order, pos2, vec2};
use egui_glow::CallbackFn;
use euclid::{Point2D, Rect, Scale, Size2D};
use log::warn;
use servo::{
    ConsoleLogLevel, Cursor, DeviceIndependentPixel, DevicePixel, LoadStatus, NavigationRequest,
    OffscreenRenderingContext, RenderingContext, Theme, WebView, WebViewBuilder, WebViewDelegate,
    WebViewId, WindowRenderingContext,
};
use url::Url;

use crate::desktop::event_loop::RepaintEventLoopWaker;
use crate::desktop::sidebar::Sidebar;
use crate::running_app_state::{RunningAppState, UserInterfaceCommand};
use crate::window::ServoShellWindow;

const MESSAGE_PREFIX: &str = "__bar__";
const PAGE_URL: &str = "servo:sidebar/commandbar.html";
/// Reopening the bar this soon after closing it (without picking anything) shows the same text.
const KEEP_TEXT: Duration = Duration::from_secs(5);
/// Space between the page and the card's rounded edge.
const CARD_PADDING: f32 = 6.0;
const CARD_RADIUS: u8 = 14;
/// Height of the bar alone, before the page reports its own.
const BAR_HEIGHT: f32 = 44.0;

struct BarDelegate {
    inbox: Rc<RefCell<Vec<String>>>,
    cursor: Rc<Cell<Cursor>>,
    waker: Waker,
}

impl WebViewDelegate for BarDelegate {
    fn show_console_message(&self, _: WebView, _: ConsoleLogLevel, message: String) {
        if let Some(message) = message.strip_prefix(MESSAGE_PREFIX) {
            self.inbox.borrow_mut().push(message.to_owned());
            self.waker.wake_by_ref();
        }
    }

    fn notify_new_frame_ready(&self, _: WebView) {
        self.waker.wake_by_ref();
    }

    fn notify_cursor_changed(&self, _: WebView, cursor: Cursor) {
        self.cursor.set(cursor);
    }

    fn request_navigation(&self, _: WebView, request: NavigationRequest) {
        if request.url.as_str() == PAGE_URL {
            request.allow();
        } else {
            request.deny();
        }
    }
}

#[derive(Default)]
pub(crate) struct CommandBar {
    webview: Option<WebView>,
    rendering_context: Option<Rc<OffscreenRenderingContext>>,
    inbox: Rc<RefCell<Vec<String>>>,
    cursor: Rc<Cell<Cursor>>,
    /// The page has loaded and can be shown.
    ready: bool,
    open: bool,
    /// Open or close it on the next update (New Tab, Ctrl+T, a click outside).
    toggle: Cell<bool>,
    close: Cell<bool>,
    /// Where the page is drawn while open, in device-independent pixels.
    rect: Cell<Option<egui::Rect>>,
    /// The page's height, as it reported it.
    height: f32,
    /// What's typed in the bar.
    text: String,
    /// The text when the bar was last closed without picking anything, and when.
    last_text: Option<(String, Instant)>,
    /// Empty New Tab pages the bar has already opened over by itself.
    auto_opened: HashSet<WebViewId>,
    /// The tab that was active when the bar opened (see `active`): switching tabs closes it.
    opened_over: Option<(WebViewId, bool)>,
}

impl CommandBar {
    /// Open the bar, or close it if it's open (on the next update).
    pub(crate) fn request_toggle(&self) {
        self.toggle.set(true);
    }

    /// Close the bar without opening anything (on the next update).
    pub(crate) fn request_close(&self) {
        self.close.set(true);
    }

    pub(crate) fn webview(&self) -> Option<&WebView> {
        self.webview.as_ref().filter(|_| self.open)
    }

    pub(crate) fn cursor(&self) -> Cursor {
        self.cursor.get()
    }

    /// `point` relative to the bar's page, if it's over it.
    pub(crate) fn local_point(
        &self,
        point: Point2D<f32, DeviceIndependentPixel>,
    ) -> Option<Point2D<f32, DeviceIndependentPixel>> {
        let rect = self.rect.get().filter(|_| self.open)?;
        rect.contains(pos2(point.x, point.y))
            .then(|| Point2D::new(point.x - rect.min.x, point.y - rect.min.y))
    }

    fn call(&self, script: &str) {
        if let Some(webview) = &self.webview {
            webview.evaluate_javascript(format!("window.bar && window.bar.{script}"), |result| {
                if let Err(error) = result {
                    warn!("command bar script failed: {error:?}");
                }
            });
        }
    }

    fn ensure_webview(
        &mut self,
        state: &RunningAppState,
        window_rendering_context: &Rc<WindowRenderingContext>,
        waker: RepaintEventLoopWaker,
        theme: Theme,
        scale: Scale<f32, DeviceIndependentPixel, DevicePixel>,
    ) {
        if let Some(webview) = &self.webview {
            webview.notify_theme_change(theme);
            return;
        }
        let rendering_context =
            Rc::new(window_rendering_context.offscreen_context(PhysicalSize::new(600, 48)));
        let delegate = Rc::new(BarDelegate {
            inbox: self.inbox.clone(),
            cursor: self.cursor.clone(),
            waker: Waker::from(Arc::new(waker)),
        });
        let webview = WebViewBuilder::new(state.servo(), rendering_context.clone())
            .url(Url::parse(PAGE_URL).expect("valid command bar URL"))
            .hidpi_scale_factor(scale)
            .delegate(delegate)
            .build();
        webview.notify_theme_change(theme);
        webview.show();
        self.webview = Some(webview);
        self.rendering_context = Some(rendering_context);
    }

    /// Tests: run JavaScript in the bar's page and log what it returns.
    pub(crate) fn debug_eval(&self, script: &str) {
        if let Some(webview) = &self.webview {
            webview.evaluate_javascript(script.to_owned(), |result| {
                crate::desktop::diagnostics::log(format!("bar eval: {result:?}"))
            });
        }
    }

    fn open(&mut self, window: &ServoShellWindow, sidebar: &mut Sidebar) {
        self.open = true;
        self.opened_over = Self::active(window);
        let text = self
            .last_text
            .take()
            .filter(|(_, when)| when.elapsed() < KEEP_TEXT)
            .map(|(text, _)| text)
            .unwrap_or_default();
        self.text = text.clone();
        sidebar.set_focused(false, window);
        if let Some(active) = window.active_webview() {
            active.set_focused(false);
        }
        if let Some(webview) = &self.webview {
            webview.show();
            webview.set_focused(true);
        }
        if self.ready {
            self.send_open();
        }
        crate::desktop::diagnostics::log("command bar: open");
    }

    fn send_open(&self) {
        let engine = crate::desktop::settings::search_engine_name();
        let options = serde_json::json!({ "text": self.text, "engine": engine });
        self.call(&format!("open({options})"));
    }

    /// Close the bar. `picked`: something was opened, so don't offer the text again.
    fn close(&mut self, window: &ServoShellWindow, picked: bool) {
        if !self.open {
            return;
        }
        self.open = false;
        self.rect.set(None);
        let text = std::mem::take(&mut self.text);
        self.last_text = (!picked && !text.trim().is_empty()).then(|| (text, Instant::now()));
        if let Some(webview) = &self.webview {
            webview.set_focused(false);
            webview.hide();
        }
        if let Some(active) = window.active_webview() {
            active.set_focused(true);
        }
        crate::desktop::diagnostics::log(format!("command bar: closed (picked: {picked})"));
    }

    /// The active tab, and whether it's an empty New Tab page (which the bar replaces, so it
    /// closes when that page is navigated elsewhere, e.g. from the sidebar's URL bar).
    fn active(window: &ServoShellWindow) -> Option<(WebViewId, bool)> {
        window.active_webview().map(|webview| {
            let empty = webview
                .url()
                .is_some_and(|url| url.as_str() == "servo:newtab");
            (webview.id(), empty)
        })
    }

    /// The active tab is an empty New Tab page: picking something loads it there.
    fn empty_tab(window: &ServoShellWindow) -> Option<WebView> {
        window.active_webview().filter(|webview| {
            webview
                .url()
                .is_some_and(|url| url.as_str() == "servo:newtab")
        })
    }

    fn open_url(&mut self, window: &ServoShellWindow, url: Url) {
        match Self::empty_tab(window) {
            Some(tab) => tab.load(url),
            None => {
                window.queue_user_interface_command(UserInterfaceCommand::NewWebViewWithUrl(url))
            },
        }
        self.close(window, true);
    }

    /// Act on requests and the page's messages. Called on every GUI update.
    pub(crate) fn update(
        &mut self,
        state: &RunningAppState,
        window: &ServoShellWindow,
        sidebar: &mut Sidebar,
        window_rendering_context: &Rc<WindowRenderingContext>,
        waker: impl FnOnce() -> RepaintEventLoopWaker,
        theme: Theme,
    ) {
        let scale = window.platform_window().hidpi_scale_factor();
        // Open by itself over an empty New Tab page (e.g. after closing a space's last tab).
        if !self.open
            && sidebar.new_tab_bar()
            && !self.toggle.get()
            && let Some(tab) = Self::empty_tab(window)
            && tab.load_status() == LoadStatus::Complete
            && self.auto_opened.insert(tab.id())
        {
            self.toggle.set(true);
        }
        if self.toggle.take() {
            if self.open {
                self.close(window, false);
            } else {
                self.ensure_webview(state, window_rendering_context, waker(), theme, scale);
                self.open(window, sidebar);
                state.history.borrow_mut().warm();
            }
        }
        if self.close.take() || (self.open && Self::active(window) != self.opened_over) {
            self.close(window, false);
        }
        let messages = std::mem::take(&mut *self.inbox.borrow_mut());
        for message in messages {
            let (command, argument) = message.split_once('\t').unwrap_or((&message, ""));
            let (first, rest) = argument.split_once('\t').unwrap_or((argument, ""));
            match command {
                "ready" => {
                    self.ready = true;
                    if self.open {
                        self.send_open();
                    }
                },
                "height" => self.height = argument.parse().unwrap_or(BAR_HEIGHT),
                // `<request number>\t<text>`
                "suggest" => {
                    self.text = rest.to_owned();
                    if rest.trim().is_empty() {
                        continue;
                    }
                    let started = Instant::now();
                    let tabs = sidebar.switch_targets(window);
                    let rows = crate::desktop::history::suggestions(
                        &mut state.history.borrow_mut(),
                        tabs,
                        rest,
                    );
                    let ms = started.elapsed().as_secs_f64() * 1000.0;
                    let reply =
                        serde_json::json!({ "rows": rows, "ms": (ms * 100.0).round() / 100.0 });
                    self.call(&format!(
                        "suggestions({}, {reply})",
                        first.parse::<u64>().unwrap_or(0)
                    ));
                },
                // `visit\t<typed text>` (what Enter does with it) or `url\t<url>`
                "open" if self.open => {
                    let target = if first == "visit" {
                        let template = crate::desktop::settings::search_template();
                        crate::parser::location_bar_input_to_url(rest, &template)
                            .map(|url| url.into_url())
                    } else {
                        Url::parse(rest).ok()
                    };
                    if let Some(url) = target {
                        self.open_url(window, url);
                    }
                },
                // `<sidebar command>\t<argument>`: show a tab that's already open.
                "switch" if self.open => {
                    if matches!(first, "activate" | "pin-open") {
                        sidebar.post(&format!("{first}\t{rest}"));
                    }
                    self.close(window, true);
                },
                "close" => self.close(window, false),
                _ => {},
            }
        }
        sidebar.set_command_bar(self.open);
    }

    /// Draw the dimmed backdrop, the card and the bar's page over the content area.
    pub(crate) fn paint(
        &self,
        ctx: &egui::Context,
        content: egui::Rect,
        scale: Scale<f32, DeviceIndependentPixel, DevicePixel>,
        theme: Theme,
    ) {
        let (Some(webview), Some(rendering_context)) = (&self.webview, &self.rendering_context)
        else {
            return;
        };
        if !self.open {
            return;
        }
        ctx.layer_painter(LayerId::new(Order::Middle, Id::new("command-bar-backdrop")))
            .rect_filled(
                content,
                CornerRadius::same(8),
                Color32::from_black_alpha(80),
            );
        // Centred in the upper third of the page, a little over half its width.
        let width = (content.width() * 0.58)
            .clamp(320.0, 760.0)
            .min(content.width() - 32.0);
        let top = content.min.y + (content.height() * 0.2).max(24.0);
        let height = self
            .height
            .max(BAR_HEIGHT)
            .min(content.max.y - top - 16.0)
            .max(1.0);
        let rect = egui::Rect::from_min_size(
            pos2((content.center().x - width / 2.0).round(), top.round()),
            vec2(width.round(), height.round()),
        );
        let card = rect.expand(CARD_PADDING);
        let fill = match theme {
            Theme::Dark => Color32::from_rgb(42, 42, 42),
            Theme::Light => Color32::WHITE,
        };
        let painter =
            ctx.layer_painter(LayerId::new(Order::Foreground, Id::new("command-bar-card")));
        painter.add(
            egui::epaint::Shadow {
                offset: [0, 6],
                blur: 24,
                spread: 0,
                color: Color32::from_black_alpha(90),
            }
            .as_shape(card, CornerRadius::same(CARD_RADIUS)),
        );
        painter.rect_filled(card, CornerRadius::same(CARD_RADIUS), fill);

        // Resize and paint the page into its own buffer, then onto the card.
        if webview.hidpi_scale_factor() != scale {
            webview.set_hidpi_scale_factor(scale);
        }
        let size = PhysicalSize::new(
            (rect.width() * scale.get()).max(1.0) as u32,
            (rect.height() * scale.get()).max(1.0) as u32,
        );
        if webview.size().to_u32() != Size2D::new(size.width, size.height) {
            webview.resize(size);
        }
        rendering_context
            .make_current()
            .expect("Could not make command bar RenderingContext current");
        webview.paint();
        rendering_context.present();
        if self.ready
            && let Some(render_to_parent) = rendering_context.render_to_parent_callback()
        {
            ctx.layer_painter(LayerId::new(Order::Tooltip, Id::new("command-bar")))
                .add(egui::PaintCallback {
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
        self.rect.set(Some(rect));
    }
}
