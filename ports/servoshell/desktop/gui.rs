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
    RenderingContext, WebViewId,
};
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, EventLoopProxy};
use winit::window::Window;

use crate::desktop::event_loop::AppEvent;
use crate::desktop::headed_window;
use crate::desktop::sidebar::{CONTENT_RADIUS, Sidebar, WindowAction, png_data_url};
use crate::desktop::updater::{self, UpdateStatus};
use crate::running_app_state::RunningAppState;
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

    /// Tabs whose page has drawn at least once (their surfaces start out white).
    painted_webviews: HashSet<WebViewId>,
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
            match fs::read(font_path) {
                Ok(bytes) => {
                    if !fonts.font_data.contains_key(*font_name) {
                        fonts
                            .font_data
                            .insert(font_name.to_string(), Arc::new(FontData::from_owned(bytes)));
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
            painted_webviews: HashSet::new(),
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

    /// If `position` is over the visible sidebar, return it relative to the sidebar's origin.
    pub(crate) fn sidebar_relative_point(
        &self,
        position: Point2D<f32, DeviceIndependentPixel>,
    ) -> Option<Point2D<f32, DeviceIndependentPixel>> {
        let rect = self.sidebar_rect?;
        rect.contains(pos2(position.x, position.y))
            .then(|| Point2D::new(position.x - rect.min.x, position.y - rect.min.y))
    }

    /// Update the user interface, but do not paint the updated state.
    pub(crate) fn update(
        &mut self,
        state: &RunningAppState,
        window: &ServoShellWindow,
        headed_window: &headed_window::HeadedWindow,
    ) {
        let theme = headed_window.theme();
        let hidpi_scale = window.platform_window().hidpi_scale_factor();
        self.sidebar.ensure_webview(state, theme, hidpi_scale);
        load_pending_favicons(window, &mut self.sidebar);
        self.sidebar.handle_messages(window);

        // Caption buttons and window dragging for the frameless window.
        let winit_window = headed_window.winit_window();
        for action in self.sidebar.take_window_actions() {
            match action {
                WindowAction::Minimize => winit_window.set_minimized(true),
                WindowAction::ToggleMaximize => {
                    winit_window.set_maximized(!winit_window.is_maximized())
                },
                WindowAction::Close => window.schedule_close(),
                WindowAction::Drag => {
                    let _ = winit_window.drag_window();
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
                        let _ = proxy.send_event(AppEvent::OpenUrl(update.page_url));
                    }
                },
            }
        }
        self.sidebar
            .set_window_state(!winit_window.is_decorated(), winit_window.is_maximized());
        self.sidebar.push_state(window);

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
            ..
        } = self;

        let winit_window = headed_window.winit_window();
        context.run(winit_window, |ctx| {
            let scale =
                Scale::<_, DeviceIndependentPixel, DevicePixel>::new(ctx.pixels_per_point());

            headed_window.for_each_active_dialog(window, |dialog| dialog.update(ctx));

            // TODO: While in fullscreen add some way to mitigate the increased phishing risk
            // when not displaying the URL bar: https://github.com/servo/servo/issues/32443
            let full_rect = ctx.available_rect_before_wrap();
            let (new_sidebar_rect, new_content_rect) =
                sidebar.layout(full_rect, headed_window.is_fullscreen_from_document());
            *sidebar_rect = new_sidebar_rect;
            *content_rect = new_content_rect;

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

            // A page's render surface is white until the page first draws; show the window
            // background instead of a white flash until then (once per tab).
            let active = window.active_webview();
            if let Some(webview) = &active &&
                webview.load_status() == LoadStatus::Complete
            {
                painted_webviews.insert(webview.id());
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
            if new_content_rect != full_rect {
                let thickness = CONTENT_RADIUS * 2.0;
                ctx.layer_painter(LayerId::new(Order::Middle, Id::new("content-corners")))
                    .rect_stroke(
                        new_content_rect.expand(thickness / 2.0),
                        CornerRadius::same((CONTENT_RADIUS + thickness / 2.0) as u8),
                        Stroke::new(thickness, background),
                        StrokeKind::Middle,
                    );
            }

            if let Some(sidebar_rect) = new_sidebar_rect {
                sidebar.paint(sidebar_rect, scale);
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
        });

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
        }
    }

    fn update_status_text(&mut self, window: &ServoShellWindow) -> bool {
        let state_status = window
            .active_webview()
            .and_then(|webview| webview.status_text());
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
        let rgba = image
            .pixels
            .iter()
            .flat_map(|pixel| pixel.to_srgba_unmultiplied())
            .collect();
        if let Some(data_url) =
            png_data_url(image.size[0] as u32, image.size[1] as u32, rgba)
        {
            sidebar.set_favicon(id, data_url);
        }
    }
}
