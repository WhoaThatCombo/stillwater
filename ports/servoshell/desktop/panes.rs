/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Extra pages shown next to (split view) or over (Glance) the active tab.
//!
//! In Servo a `WebView` is tied to the rendering context it was created with, and each
//! context shows one page at a time, so every visible extra pane gets its own offscreen
//! rendering context. Panes are created on demand and dropped (freeing their renderer)
//! when closed.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::task::Waker;
use std::time::{Duration, Instant};

use dpi::PhysicalSize;
use euclid::Scale;
use servo::{
    Cursor, DeviceIndependentPixel, DevicePixel, LoadStatus, NavigationRequest,
    OffscreenRenderingContext, RenderingContext, Theme, WebView, WebViewBuilder, WebViewDelegate,
    WindowRenderingContext,
};
use url::Url;

use crate::desktop::event_loop::RepaintEventLoopWaker;
use crate::running_app_state::RunningAppState;

thread_local! {
    /// Set by an Alt+click on the page: the navigation it causes opens in Glance instead.
    static GLANCE_ARMED: Cell<Option<Instant>> = const { Cell::new(None) };
    /// A link to open in Glance on the next GUI update.
    static GLANCE_URL: RefCell<Option<Url>> = const { RefCell::new(None) };
}

/// The next navigation of the active tab (within a moment) opens in Glance.
pub(crate) fn arm_glance() {
    GLANCE_ARMED.with(|armed| armed.set(Some(Instant::now() + Duration::from_millis(1500))));
}

/// If a Glance is armed, divert `url` into it and return true (the navigation is denied).
pub(crate) fn divert_to_glance(url: &Url) -> bool {
    let armed = GLANCE_ARMED.with(|armed| armed.take()).is_some_and(|until| Instant::now() < until);
    if armed && matches!(url.scheme(), "http" | "https") {
        GLANCE_URL.with(|pending| *pending.borrow_mut() = Some(url.clone()));
        return true;
    }
    false
}

pub(crate) fn take_glance_url() -> Option<Url> {
    GLANCE_URL.with(|pending| pending.borrow_mut().take())
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum PaneKind {
    Split,
    Glance,
}

/// State the pane's delegate shares with the window.
struct PaneShared {
    cursor: Cell<Cursor>,
    waker: Waker,
}

struct PaneDelegate {
    shared: Rc<PaneShared>,
    downloads: crate::desktop::downloads::SharedDownloads,
}

impl PaneDelegate {
    /// Title, URL or load state changed: redraw so the sidebar picks it up.
    fn changed(&self) {
        self.shared.waker.wake_by_ref();
    }
}

impl WebViewDelegate for PaneDelegate {
    fn notify_new_frame_ready(&self, _: WebView) {
        self.shared.waker.wake_by_ref();
    }

    fn notify_url_changed(&self, _: WebView, _: Url) {
        self.changed();
    }

    fn notify_page_title_changed(&self, _: WebView, _: Option<String>) {
        self.changed();
    }

    fn notify_load_status_changed(&self, _: WebView, _: LoadStatus) {
        self.changed();
    }

    fn notify_history_changed(&self, _: WebView, _: Vec<Url>, _: usize) {
        self.changed();
    }

    fn notify_cursor_changed(&self, _: WebView, cursor: Cursor) {
        self.shared.cursor.set(cursor);
    }

    fn request_navigation(&self, _: WebView, request: NavigationRequest) {
        request.allow();
    }

    fn request_download(&self, _: WebView, request: servo::DownloadRequest) {
        self.downloads.borrow_mut().start(request);
        self.changed();
    }
}

pub(crate) struct Pane {
    pub(crate) webview: WebView,
    rendering_context: Rc<OffscreenRenderingContext>,
    shared: Rc<PaneShared>,
}

impl Pane {
    pub(crate) fn new(
        kind: PaneKind,
        url: Url,
        state: &RunningAppState,
        window_rendering_context: &Rc<WindowRenderingContext>,
        waker: RepaintEventLoopWaker,
        theme: Theme,
        scale: Scale<f32, DeviceIndependentPixel, DevicePixel>,
    ) -> Self {
        let rendering_context = Rc::new(
            window_rendering_context.offscreen_context(PhysicalSize::new(800, 600)),
        );
        let shared = Rc::new(PaneShared {
            cursor: Cell::new(Cursor::Default),
            waker: Waker::from(Arc::new(waker)),
        });
        let webview = WebViewBuilder::new(state.servo(), rendering_context.clone())
            .url(url)
            .hidpi_scale_factor(scale)
            .user_content_manager(state.user_content_manager.clone())
            .clipboard_delegate(crate::desktop::clipboard::delegate())
            .delegate(Rc::new(PaneDelegate {
                shared: shared.clone(),
                downloads: state.downloads.clone(),
            }))
            .build();
        webview.notify_theme_change(theme);
        webview.show();
        let _ = kind;
        Self {
            webview,
            rendering_context,
            shared,
        }
    }

    /// Resize and paint the pane into its own offscreen buffer.
    pub(crate) fn paint(&self, rect: egui::Rect, scale: Scale<f32, DeviceIndependentPixel, DevicePixel>) {
        if self.webview.hidpi_scale_factor() != scale {
            self.webview.set_hidpi_scale_factor(scale);
        }
        let size = PhysicalSize::new(
            (rect.width() * scale.get()).max(1.0) as u32,
            (rect.height() * scale.get()).max(1.0) as u32,
        );
        if self.webview.size().to_u32() != euclid::Size2D::new(size.width, size.height) {
            self.webview.resize(size);
        }
        self.rendering_context
            .make_current()
            .expect("Could not make pane RenderingContext current");
        self.webview.paint();
        self.rendering_context.present();
    }

    pub(crate) fn rendering_context(&self) -> &Rc<OffscreenRenderingContext> {
        &self.rendering_context
    }

    pub(crate) fn cursor(&self) -> Cursor {
        self.shared.cursor.get()
    }


    pub(crate) fn url(&self) -> Option<Url> {
        self.webview.url()
    }

    pub(crate) fn title(&self) -> String {
        self.webview
            .page_title()
            .filter(|title| !title.is_empty())
            .or_else(|| self.webview.url().map(|url| url.to_string()))
            .unwrap_or_default()
    }
}
