/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Overlay scrollbars for the page viewport, like Windows 11's and Firefox's: a thin thumb at
//! the right (and bottom) edge of each page view (the active tab, the split pane, Glance)
//! that shows while the page scrolls, widens when the pointer comes near, can be dragged, and
//! fades out. They are drawn by egui over the page, so they never change the page's layout,
//! and read the scroll state straight from Servo's painter (`WebView::root_scroll_state`), so
//! they follow every kind of scrolling (wheel, keys, find, anchors, script) without asking
//! the page anything. Only the page's own viewport has a bar; boxes inside the page that
//! scroll do not (yet).

use std::cell::Cell;
use std::collections::HashMap;
use std::time::{Duration, Instant};

use egui::{Color32, CornerRadius, LayerId};
use servo::{RootScrollState, WebView, WebViewId};

use crate::desktop::scrollbar_geometry::{self as geometry, X, Y};

/// The "Scrollbar" setting.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum ScrollbarMode {
    /// Shows while scrolling or when the pointer is near the edge, then fades out.
    #[default]
    Auto,
    Always,
    Hidden,
}

impl ScrollbarMode {
    pub(crate) fn from_id(id: &str) -> Option<Self> {
        match id {
            "auto" => Some(Self::Auto),
            "always" => Some(Self::Always),
            "hidden" => Some(Self::Hidden),
            _ => None,
        }
    }

    pub(crate) fn id(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Always => "always",
            Self::Hidden => "hidden",
        }
    }
}

thread_local! {
    static MODE: Cell<ScrollbarMode> = Cell::new(initial_mode());
    /// When a fading bar next needs a frame (see `next_wake`).
    static WAKE: Cell<Option<Instant>> = const { Cell::new(None) };
}

/// Tests can force a mode without touching the saved settings.
fn initial_mode() -> ScrollbarMode {
    std::env::var("STILLWATER_SCROLLBAR")
        .ok()
        .and_then(|id| ScrollbarMode::from_id(&id))
        .unwrap_or_default()
}

pub(crate) fn set_mode(mode: ScrollbarMode) {
    if std::env::var_os("STILLWATER_SCROLLBAR").is_none() {
        MODE.with(|cell| cell.set(mode));
    }
}

fn mode() -> ScrollbarMode {
    MODE.with(Cell::get)
}

/// When the event loop must wake to animate a bar (its fade-out, or a held track press), if
/// ever. Nothing is scheduled once every bar has faded, so an idle page costs no frames.
pub(crate) fn next_wake() -> Option<Instant> {
    WAKE.with(Cell::get).filter(|at| *at > Instant::now())
}

/// Ask for a frame at `at` (the earliest pending request wins; past ones are dropped).
fn wake_at(at: Instant) {
    WAKE.with(|cell| {
        let now = Instant::now();
        let current = cell.get().filter(|current| *current > now);
        cell.set(Some(current.map_or(at, |current| current.min(at))));
    });
}

/// The frame interval while a bar fades.
const FADE_FRAME: Duration = Duration::from_millis(16);
/// A held press on the track pages once, then again after this, and then at `REPEAT_EVERY`.
const REPEAT_DELAY: Duration = Duration::from_millis(350);
const REPEAT_EVERY: Duration = Duration::from_millis(60);

/// A page view laid out in the last frame.
struct View {
    webview: WebView,
    rect: geometry::Rect,
    /// Which axes have a bar.
    axes: [bool; 2],
}

impl View {
    fn both(&self) -> bool {
        self.axes[X] && self.axes[Y]
    }
}

/// What a page view's bars remember between frames.
struct Memory {
    offset: [f32; 2],
    /// The last scroll or hover, from which the bars fade out.
    activity: Option<Instant>,
}

struct Drag {
    webview: WebViewId,
    axis: usize,
    /// Where on the thumb (from its start) it was grabbed.
    grab: f32,
}

struct Repeat {
    webview: WebViewId,
    axis: usize,
    direction: i8,
    next: Instant,
}

/// What a pointer move meant for the bars.
pub(crate) struct Moved {
    /// The move is the bars' (dragging, or over a bar): the page must not see it.
    pub consumed: bool,
    /// The bars look different now: redraw.
    pub redraw: bool,
    /// The pointer just went onto a bar from this page, which should forget its hover state.
    pub left_page: Option<WebView>,
}

#[derive(Default)]
pub(crate) struct Scrollbars {
    views: Vec<View>,
    memory: HashMap<WebViewId, Memory>,
    hover: Option<(WebViewId, usize)>,
    /// The page whose press zone the pointer is in.
    over: Option<WebViewId>,
    drag: Option<Drag>,
    repeat: Option<Repeat>,
    pointer: Option<[f32; 2]>,
}

impl Scrollbars {
    fn view(&self, id: WebViewId) -> Option<&View> {
        self.views.iter().find(|view| view.webview.id() == id)
    }

    /// Whether the bar of `axis` on page `id` is hovered, dragged or held.
    fn engaged(&self, id: WebViewId, axis: usize) -> bool {
        self.hover == Some((id, axis)) ||
            self.drag.as_ref().is_some_and(|drag| drag.webview == id && drag.axis == axis) ||
            self.repeat.as_ref().is_some_and(|repeat| repeat.webview == id && repeat.axis == axis)
    }

    /// Whether the scrollbar owns the mouse (a drag or held track press is under way).
    pub(crate) fn captured(&self) -> bool {
        self.drag.is_some() || self.repeat.is_some()
    }

    /// Lay out and draw the bars of `views` (each page with where it is drawn and the egui
    /// layer to draw its bars on). Called on every frame.
    pub(crate) fn paint(
        &mut self,
        ctx: &egui::Context,
        views: &[(WebView, egui::Rect, LayerId)],
    ) {
        let now = Instant::now();
        let mode = mode();
        self.views.clear();
        if mode == ScrollbarMode::Hidden {
            self.cancel();
            self.hover = None;
            self.over = None;
            return;
        }
        self.memory
            .retain(|id, _| views.iter().any(|(webview, ..)| webview.id() == *id));
        self.step_repeat(now);

        for (webview, rect, layer) in views {
            let Some(state) = webview.root_scroll_state() else {
                continue;
            };
            let id = webview.id();
            let view_rect = [rect.min.x, rect.min.y, rect.max.x, rect.max.y];
            let axes = [X, Y].map(|axis| {
                state.user_scrollable[axis] &&
                    geometry::scrollable(state.viewport_size[axis], state.content_size[axis])
            });
            let engaged = [X, Y].map(|axis| axes[axis] && self.engaged(id, axis));
            let memory = self.memory.entry(id).or_insert(Memory {
                offset: state.offset,
                activity: None,
            });
            if memory.offset != state.offset {
                memory.offset = state.offset;
                memory.activity = Some(now);
            }
            if engaged[X] || engaged[Y] {
                memory.activity = Some(now);
            }
            let opacity = match mode {
                ScrollbarMode::Always => 1.0,
                _ => memory.activity.map_or(0.0, |at| {
                    let since = now.duration_since(at).as_secs_f32();
                    if since < geometry::SHOW_FOR {
                        wake_at(at + Duration::from_secs_f32(geometry::SHOW_FOR));
                    } else if since < geometry::SHOW_FOR + geometry::FADE_FOR {
                        wake_at(now + FADE_FRAME);
                    }
                    geometry::auto_hide_opacity(since)
                }),
            };
            let view = View {
                webview: webview.clone(),
                rect: view_rect,
                axes,
            };
            if opacity > 0.0 {
                let dark = page_is_dark(&state);
                for axis in [Y, X] {
                    if axes[axis] {
                        draw_bar(ctx, *layer, &view, &state, axis, engaged[axis], opacity, dark);
                    }
                }
            }
            self.views.push(view);
        }
        if let Some(repeat) = &self.repeat {
            wake_at(repeat.next);
        }
    }

    /// The bar (page and axis) whose press zone contains `point`.
    fn press_target(&self, point: [f32; 2]) -> Option<(&View, usize)> {
        self.views.iter().find_map(|view| {
            [Y, X]
                .into_iter()
                .find(|&axis| {
                    view.axes[axis] &&
                        geometry::contains(geometry::press_zone(view.rect, axis, view.both()), point)
                })
                .map(|axis| (view, axis))
        })
    }

    /// Whether `(x, y)` is on a bar (where a press would be the bar's).
    pub(crate) fn over(&self, x: f32, y: f32) -> bool {
        self.press_target([x, y]).is_some()
    }

    /// The pointer moved to `point` (window coordinates, device-independent pixels).
    pub(crate) fn pointer_moved(&mut self, point: [f32; 2]) -> Moved {
        self.pointer = Some(point);
        if let Some(drag) = &self.drag {
            if let Some(view) = self.view(drag.webview) {
                drag_to(view, drag.axis, drag.grab, point);
            }
            return Moved {
                consumed: true,
                redraw: true,
                left_page: None,
            };
        }
        if self.repeat.is_some() {
            return Moved {
                consumed: true,
                redraw: false,
                left_page: None,
            };
        }
        let hover = self.views.iter().find_map(|view| {
            [Y, X]
                .into_iter()
                .find(|&axis| {
                    view.axes[axis] && geometry::contains(geometry::hover_zone(view.rect, axis), point)
                })
                .map(|axis| (view.webview.id(), axis))
        });
        let redraw = self.hover != hover;
        self.hover = hover;
        let over = self.press_target(point).map(|(view, _)| view.webview.clone());
        let previous = std::mem::replace(&mut self.over, over.as_ref().map(WebView::id));
        Moved {
            consumed: over.is_some(),
            redraw,
            left_page: over.filter(|view| previous != Some(view.id())),
        }
    }

    /// The pointer left the window (or the pages): stop widening.
    pub(crate) fn pointer_left(&mut self) -> bool {
        self.over = None;
        self.hover.take().is_some()
    }

    /// The left button went down at `point`. Returns true if it was on a bar (the press is
    /// then the bar's: a drag of the thumb, or paging on the track).
    pub(crate) fn pressed(&mut self, point: [f32; 2]) -> bool {
        let Some((view, axis)) = self.press_target(point) else {
            return false;
        };
        let Some(state) = view.webview.root_scroll_state() else {
            return false;
        };
        let id = view.webview.id();
        let track = geometry::track_rect(view.rect, axis, view.both(), geometry::WIDE);
        let length = geometry::track_length(track, axis);
        let Some((start, thumb_length)) = geometry::thumb(
            length,
            state.viewport_size[axis],
            state.content_size[axis],
            state.offset[axis],
        ) else {
            return false;
        };
        let along = point[axis] - track[axis];
        match geometry::track_click(start, thumb_length, along) {
            0 => {
                self.drag = Some(Drag {
                    webview: id,
                    axis,
                    grab: along - start,
                });
            },
            direction => {
                page(&view.webview, &state, axis, direction);
                self.repeat = Some(Repeat {
                    webview: id,
                    axis,
                    direction,
                    next: Instant::now() + REPEAT_DELAY,
                });
            },
        }
        self.hover = Some((id, axis));
        if let Some(memory) = self.memory.get_mut(&id) {
            memory.activity = Some(Instant::now());
        }
        true
    }

    /// The left button went up (or the window lost focus). Returns true if a drag or track
    /// press ended, so the release is the bar's.
    pub(crate) fn released(&mut self) -> bool {
        let was = self.captured();
        self.cancel();
        was
    }

    fn cancel(&mut self) {
        self.drag = None;
        self.repeat = None;
    }

    /// Keep paging while the track is held, until the thumb reaches the pointer.
    fn step_repeat(&mut self, now: Instant) {
        let Some(repeat) = &mut self.repeat else {
            return;
        };
        if now < repeat.next {
            return;
        }
        repeat.next = now + REPEAT_EVERY;
        let (id, axis, direction) = (repeat.webview, repeat.axis, repeat.direction);
        let Some(view) = self.view(id) else {
            return;
        };
        let (Some(pointer), Some(state)) = (self.pointer, view.webview.root_scroll_state()) else {
            return;
        };
        let track = geometry::track_rect(view.rect, axis, view.both(), geometry::WIDE);
        let Some((start, length)) = geometry::thumb(
            geometry::track_length(track, axis),
            state.viewport_size[axis],
            state.content_size[axis],
            state.offset[axis],
        ) else {
            return;
        };
        if geometry::track_click(start, length, pointer[axis] - track[axis]) == direction {
            page(&view.webview, &state, axis, direction);
        }
    }
}

/// Scroll one page towards `direction` on `axis`.
fn page(webview: &WebView, state: &RootScrollState, axis: usize, direction: i8) {
    let mut delta = [0.0; 2];
    delta[axis] = f32::from(direction) * state.viewport_size[axis] * geometry::PAGE_FRACTION;
    webview.scroll_viewport_by(delta);
}

/// Scroll so that the thumb grabbed `grab` pixels from its start is under `point`.
fn drag_to(view: &View, axis: usize, grab: f32, point: [f32; 2]) {
    let Some(state) = view.webview.root_scroll_state() else {
        return;
    };
    let track = geometry::track_rect(view.rect, axis, view.both(), geometry::WIDE);
    let target = geometry::drag_offset(
        geometry::track_length(track, axis),
        state.viewport_size[axis],
        state.content_size[axis],
        point[axis] - track[axis],
        grab,
    );
    let mut delta = [0.0; 2];
    delta[axis] = target - state.offset[axis];
    if delta[axis].abs() >= 0.01 {
        view.webview.scroll_viewport_by(delta);
    }
}

/// Whether the page under the bar is dark, so the thumb should be light: its own background
/// as painted over the canvas colour behind every page (white by default; Servo doesn't darken
/// it for the dark theme, so the theme alone says nothing about the page).
fn page_is_dark(state: &RootScrollState) -> bool {
    let canvas = servo::pref!(shell_background_color_rgba);
    let [r, g, b, a] = state.background.unwrap_or([0.0; 4]);
    let mix = |page: f32, under: f64| page * a + under as f32 * (1.0 - a);
    geometry::is_dark([mix(r, canvas[0]), mix(g, canvas[1]), mix(b, canvas[2])])
}

#[allow(clippy::too_many_arguments)]
fn draw_bar(
    ctx: &egui::Context,
    layer: LayerId,
    view: &View,
    state: &RootScrollState,
    axis: usize,
    engaged: bool,
    opacity: f32,
    dark: bool,
) {
    let widen = ctx.animate_bool_with_time(
        egui::Id::new(("stillwater-scrollbar", view.webview.id(), axis)),
        engaged,
        0.12,
    );
    let width = geometry::THIN + (geometry::WIDE - geometry::THIN) * widen;
    let track = geometry::track_rect(view.rect, axis, view.both(), width);
    let Some(thumb) = geometry::thumb(
        geometry::track_length(track, axis),
        state.viewport_size[axis],
        state.content_size[axis],
        state.offset[axis],
    ) else {
        return;
    };
    let thumb = geometry::thumb_rect(track, axis, thumb);
    let rect = |r: geometry::Rect| egui::Rect::from_min_max(egui::pos2(r[0], r[1]), egui::pos2(r[2], r[3]));
    let ink = |alpha: f32| {
        let alpha = (alpha * opacity * 255.0).round() as u8;
        if dark {
            Color32::from_white_alpha(alpha)
        } else {
            Color32::from_black_alpha(alpha)
        }
    };
    let radius = CornerRadius::same((width / 2.0).round() as u8);
    let painter = ctx.layer_painter(layer).with_clip_rect(rect(view.rect));
    if widen > 0.0 {
        painter.rect_filled(rect(track), radius, ink(0.10 * widen));
    }
    let thumb_alpha = if dark { 0.45 } else { 0.40 } + 0.15 * widen;
    painter.rect_filled(rect(thumb), radius, ink(thumb_alpha));
}
