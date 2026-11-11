/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! The find bar (Ctrl+F): a small panel floating at the top-right of the page, driving
//! `WebView::find_in_page`.
//!
//! It is drawn by egui rather than by the sidebar page so that it sits next to the page it
//! searches (also in compact mode, where the sidebar is hidden, and over a split pane), and
//! so that each keystroke reaches the search directly instead of going through the sidebar's
//! web page first. Highlighting and scrolling are done by the engine; nothing is injected
//! into the page.

use std::cell::RefCell;
use std::collections::HashMap;
use std::time::Instant;

use egui::text::{CCursor, CCursorRange};
use egui::widgets::text_edit::TextEditState;
use egui::{
    Color32, CornerRadius, FontId, Id, Key, Modifiers, Order, Sense, Stroke, StrokeKind, pos2, vec2,
};
use servo::{FindInPageOptions, FindInPageResult, LoadStatus, WebView, WebViewId};

use crate::desktop::diagnostics;
use crate::window::ServoShellWindow;

/// What the find bar has asked a page to find. The search is sent again when any of this
/// changes, e.g. after the page navigates or finishes loading.
#[derive(Clone, PartialEq)]
struct Search {
    webview: WebViewId,
    query: String,
    case_sensitive: bool,
    url: Option<url::Url>,
    loaded: bool,
}

/// A find bar command from the keyboard, the sidebar or a test script.
#[derive(Clone, Debug)]
pub(crate) enum FindCommand {
    /// Ctrl+F: show the bar and select its text.
    Open,
    /// Replace the search text (as if typed).
    Query(String),
    /// Enter / F3 (`true`) or Shift+Enter / Shift+F3 (`false`).
    Step(bool),
    MatchCase(bool),
    /// Escape.
    Close,
}

impl FindCommand {
    /// Parse a `find-*` sidebar command.
    pub(crate) fn parse(command: &str, argument: &str) -> Option<Self> {
        Some(match command {
            "find-open" => Self::Open,
            "find-query" => Self::Query(argument.to_owned()),
            "find-next" => Self::Step(true),
            "find-previous" => Self::Step(false),
            "find-match-case" => Self::MatchCase(argument == "1"),
            "find-close" => Self::Close,
            _ => return None,
        })
    }
}

#[derive(Default)]
struct State {
    open: bool,
    query: String,
    case_sensitive: bool,
    /// The search the page was last given, if any.
    sent: Option<Search>,
    /// When the last request was sent, to log how long the page took to answer.
    sent_at: Option<Instant>,
    /// The latest result for `sent`.
    result: Option<FindInPageResult>,
    /// Focus the text field and select its text on the next frame.
    focus: bool,
    /// Move to the next (`true`) or previous match on the next frame.
    step: Option<bool>,
    /// Where the bar was drawn last, while it's open.
    rect: Option<egui::Rect>,
}

thread_local! {
    /// Find results reported by pages, waiting for the next GUI update.
    static RESULTS: RefCell<HashMap<WebViewId, FindInPageResult>> = RefCell::default();
}

/// Called from `WebViewDelegate::notify_find_in_page_result`.
pub(crate) fn record_result(webview: WebViewId, result: FindInPageResult) {
    RESULTS.with(|results| results.borrow_mut().insert(webview, result));
}

#[derive(Default)]
pub(crate) struct FindBar {
    state: RefCell<State>,
}

const INPUT_ID: &str = "find-bar-input";
const BAR_WIDTH: f32 = 372.0;
const BAR_HEIGHT: f32 = 40.0;
const MARGIN: f32 = 10.0;

impl FindBar {
    pub(crate) fn handle(&self, command: FindCommand) {
        diagnostics::log(format!("find bar: {command:?}"));
        let mut state = self.state.borrow_mut();
        match command {
            FindCommand::Open => {
                state.open = true;
                state.focus = true;
            },
            FindCommand::Query(query) => {
                state.open = true;
                state.query = query;
            },
            FindCommand::Step(forward) => {
                if !state.open {
                    state.open = true;
                    state.focus = true;
                }
                state.step = Some(forward);
            },
            FindCommand::MatchCase(case_sensitive) => state.case_sensitive = case_sensitive,
            FindCommand::Close => state.open = false,
        }
    }

    /// Whether `point` is on the bar (so mouse events go to egui, not the page).
    pub(crate) fn contains(&self, point: egui::Pos2) -> bool {
        self.state
            .borrow()
            .rect
            .is_some_and(|rect| rect.expand(2.0).contains(point))
    }

    /// Draw the bar over `content_rect` (the page `webview` is shown in) and keep the page's
    /// search in sync with it. Called every GUI update.
    pub(crate) fn update(
        &self,
        ctx: &egui::Context,
        window: &ServoShellWindow,
        webview: Option<WebView>,
        content_rect: egui::Rect,
        theme: servo::Theme,
    ) {
        let mut state = self.state.borrow_mut();
        let results = RESULTS.with(|results| std::mem::take(&mut *results.borrow_mut()));
        if let Some(sent) = &state.sent &&
            let Some(result) = results.get(&sent.webview)
        {
            if let Some(sent_at) = state.sent_at.take() {
                diagnostics::log(format!(
                    "find result: {} matches, active {:?}{}, {} ms",
                    result.match_count,
                    result.active_match.map(|index| index + 1),
                    if result.limit_reached {
                        ", limit reached"
                    } else {
                        ""
                    },
                    sent_at.elapsed().as_millis(),
                ));
            }
            state.result = Some(*result);
        }

        if !state.open || webview.is_none() {
            if let Some(sent) = state.sent.take() &&
                let Some(webview) = window.webview_by_id(sent.webview)
            {
                webview.stop_finding();
            }
            state.result = None;
            state.rect = None;
            state.step = None;
            return;
        }
        let Some(webview) = webview else {
            return;
        };

        let rect = egui::Rect::from_min_size(
            pos2(
                (content_rect.max.x - BAR_WIDTH - MARGIN).max(content_rect.min.x),
                content_rect.min.y + MARGIN,
            ),
            vec2(BAR_WIDTH.min(content_rect.width()), BAR_HEIGHT),
        );
        state.rect = Some(rect);
        let mut closed = false;
        let mut step = state.step.take();
        draw(ctx, rect, theme, &mut state, &mut step, &mut closed);
        if closed {
            state.open = false;
            if let Some(sent) = state.sent.take() &&
                let Some(webview) = window.webview_by_id(sent.webview)
            {
                webview.stop_finding();
            }
            state.result = None;
            state.rect = None;
            ctx.memory_mut(|memory| memory.surrender_focus(Id::new(INPUT_ID)));
            return;
        }

        // Keep the page's search in sync with the bar.
        let wanted = (!state.query.is_empty()).then(|| Search {
            webview: webview.id(),
            query: state.query.clone(),
            case_sensitive: state.case_sensitive,
            url: webview.url(),
            loaded: webview.load_status() == LoadStatus::Complete,
        });
        if wanted != state.sent {
            if let Some(sent) = &state.sent &&
                wanted
                    .as_ref()
                    .is_none_or(|wanted| wanted.webview != sent.webview) &&
                let Some(old) = window.webview_by_id(sent.webview)
            {
                old.stop_finding();
            }
            match &wanted {
                Some(search) => {
                    webview.find_in_page(
                        &search.query,
                        FindInPageOptions {
                            case_sensitive: search.case_sensitive,
                        },
                    );
                    state.sent_at = Some(Instant::now());
                },
                None => {
                    webview.stop_finding();
                    state.result = None;
                },
            }
            state.sent = wanted;
        } else if let Some(forward) = step &&
            state.sent.is_some()
        {
            if forward {
                webview.find_next();
            } else {
                webview.find_previous();
            }
            state.sent_at = Some(Instant::now());
        }
    }
}

/// Draw the bar: the search field, the match counter, "Match case" and the previous, next
/// and close buttons. Keys pressed in the field can set `step` or `closed`.
fn draw(
    ctx: &egui::Context,
    rect: egui::Rect,
    theme: servo::Theme,
    state: &mut State,
    step: &mut Option<bool>,
    closed: &mut bool,
) {
    // The same palette as the pane buttons.
    let (fill, hover, foreground, muted, field) = match theme {
        servo::Theme::Dark => (
            Color32::from_rgb(43, 42, 51),
            Color32::from_rgb(62, 61, 72),
            Color32::from_rgb(235, 235, 240),
            Color32::from_rgb(160, 160, 170),
            Color32::from_rgb(28, 27, 34),
        ),
        servo::Theme::Light => (
            Color32::from_rgb(250, 250, 252),
            Color32::from_rgb(232, 232, 238),
            Color32::from_rgb(30, 30, 36),
            Color32::from_rgb(105, 105, 115),
            Color32::WHITE,
        ),
    };
    let accent = Color32::from_rgb(255, 150, 50);

    let input_id = Id::new(INPUT_ID);
    egui::Area::new(Id::new("find-bar"))
        .order(Order::Foreground)
        .fixed_pos(rect.min)
        .show(ctx, |ui| {
            let painter = ui.painter().clone();
            painter.rect_filled(
                rect.translate(vec2(0.0, 2.0)).expand(1.0),
                CornerRadius::same(12),
                Color32::from_black_alpha(30),
            );
            painter.rect_filled(rect, CornerRadius::same(10), fill);
            painter.rect_stroke(
                rect,
                CornerRadius::same(10),
                Stroke::new(1.0_f32, Color32::from_black_alpha(40)),
                StrokeKind::Inside,
            );
            // Swallow clicks on the bar's background.
            ui.interact(rect, Id::new("find-bar-background"), Sense::click());

            // Keys for the bar, taken before the text field sees them.
            let focused = ui.memory(|memory| memory.has_focus(input_id));
            if focused {
                ui.input_mut(|input| {
                    if input.consume_key(Modifiers::SHIFT, Key::Enter) ||
                        input.consume_key(Modifiers::SHIFT, Key::F3)
                    {
                        *step = Some(false);
                    } else if input.consume_key(Modifiers::NONE, Key::Enter) ||
                        input.consume_key(Modifiers::NONE, Key::F3)
                    {
                        *step = Some(true);
                    }
                    if input.consume_key(Modifiers::NONE, Key::Escape) {
                        *closed = true;
                    }
                    if input.consume_key(Modifiers::COMMAND, Key::F) {
                        state.focus = true;
                    }
                });
            }

            // The search field.
            let button = 28.0;
            let field_rect = egui::Rect::from_min_max(
                pos2(rect.min.x + 8.0, rect.min.y + 6.0),
                pos2(
                    rect.max.x - 8.0 - 4.0 * (button + 2.0) - 70.0,
                    rect.max.y - 6.0,
                ),
            );
            painter.rect_filled(field_rect, CornerRadius::same(6), field);
            let has_focus = ui.memory(|memory| memory.has_focus(input_id));
            painter.rect_stroke(
                field_rect,
                CornerRadius::same(6),
                Stroke::new(
                    1.0_f32,
                    if has_focus {
                        accent
                    } else {
                        Color32::from_black_alpha(50)
                    },
                ),
                StrokeKind::Inside,
            );
            let edit = egui::TextEdit::singleline(&mut state.query)
                .id(input_id)
                .frame(egui::Frame::NONE)
                .font(FontId::proportional(14.0))
                .text_color(foreground)
                .hint_text(egui::RichText::new("Find in page").color(muted))
                .return_key(None)
                .desired_width(field_rect.width() - 16.0);
            let response = ui.put(field_rect.shrink2(vec2(8.0, 4.0)), edit);
            // Right-click: Cut, Copy, Paste, Select all (like the sidebar's text fields).
            if response.secondary_clicked() {
                let clipboard = crate::desktop::clipboard::get_text().unwrap_or_default();
                ui.data_mut(|data| data.insert_temp(input_id.with("clipboard"), clipboard));
            }
            response.context_menu(|ui| text_field_menu(ui, input_id, &mut state.query));
            if state.focus {
                state.focus = false;
                response.request_focus();
                if let Some(mut edit_state) = TextEditState::load(ctx, input_id) {
                    let end = state.query.chars().count();
                    edit_state.cursor.set_char_range(Some(CCursorRange::two(
                        CCursor::new(0),
                        CCursor::new(end),
                    )));
                    edit_state.store(ctx, input_id);
                }
            }

            // The match counter.
            let counter = match state.result {
                _ if state.query.is_empty() => String::new(),
                None => String::new(),
                Some(result) if result.match_count == 0 => "No results".to_owned(),
                Some(result) => format!(
                    "{} of {}{}",
                    result.active_match.map_or(0, |index| index + 1),
                    result.match_count,
                    if result.limit_reached { "+" } else { "" }
                ),
            };
            painter.text(
                pos2(field_rect.max.x + 8.0, rect.center().y),
                egui::Align2::LEFT_CENTER,
                counter,
                FontId::proportional(12.5),
                muted,
            );

            // Match case, previous, next, close.
            let mut x = rect.max.x - 8.0 - 4.0 * (button + 2.0) + 2.0;
            for (index, tooltip) in [
                "Match case",
                "Previous match (Shift+Enter)",
                "Next match (Enter)",
                "Close (Esc)",
            ]
            .into_iter()
            .enumerate()
            {
                let button_rect = egui::Rect::from_min_size(
                    pos2(x, rect.center().y - button / 2.0),
                    vec2(button, button),
                );
                x += button + 2.0;
                let response = ui
                    .interact(
                        button_rect,
                        Id::new(("find-bar-button", index)),
                        Sense::click(),
                    )
                    .on_hover_text(tooltip);
                let active = index == 0 && state.case_sensitive;
                if response.hovered() || active {
                    painter.rect_filled(button_rect, CornerRadius::same(6), hover);
                }
                if active {
                    painter.rect_stroke(
                        button_rect,
                        CornerRadius::same(6),
                        Stroke::new(1.0_f32, accent),
                        StrokeKind::Inside,
                    );
                }
                let stroke = Stroke::new(1.5_f32, foreground);
                let c = button_rect.center();
                match index {
                    0 => {
                        painter.text(
                            c,
                            egui::Align2::CENTER_CENTER,
                            "Aa",
                            FontId::proportional(13.0),
                            foreground,
                        );
                    },
                    1 => {
                        painter.line_segment(
                            [pos2(c.x - 5.0, c.y + 2.5), pos2(c.x, c.y - 2.5)],
                            stroke,
                        );
                        painter.line_segment(
                            [pos2(c.x, c.y - 2.5), pos2(c.x + 5.0, c.y + 2.5)],
                            stroke,
                        );
                    },
                    2 => {
                        painter.line_segment(
                            [pos2(c.x - 5.0, c.y - 2.5), pos2(c.x, c.y + 2.5)],
                            stroke,
                        );
                        painter.line_segment(
                            [pos2(c.x, c.y + 2.5), pos2(c.x + 5.0, c.y - 2.5)],
                            stroke,
                        );
                    },
                    _ => {
                        painter.line_segment(
                            [pos2(c.x - 4.5, c.y - 4.5), pos2(c.x + 4.5, c.y + 4.5)],
                            stroke,
                        );
                        painter.line_segment(
                            [pos2(c.x - 4.5, c.y + 4.5), pos2(c.x + 4.5, c.y - 4.5)],
                            stroke,
                        );
                    },
                }
                if response.clicked() {
                    match index {
                        0 => state.case_sensitive = !state.case_sensitive,
                        1 => *step = Some(false),
                        2 => *step = Some(true),
                        _ => *closed = true,
                    }
                    // Keep typing in the field after using a button.
                    if index < 3 {
                        response.request_focus();
                        ui.memory_mut(|memory| memory.request_focus(input_id));
                    }
                }
            }
        });
}

/// The right-click menu of an egui text field: Cut, Copy, Paste, Select all, acting on the
/// selection (Cut and Copy on the whole text when nothing is selected). The clipboard's text is
/// read once, when the menu opens (see the caller).
fn text_field_menu(ui: &mut egui::Ui, input_id: Id, text: &mut String) {
    let ctx = ui.ctx().clone();
    let length = text.chars().count();
    let (start, end) = TextEditState::load(&ctx, input_id)
        .and_then(|state| state.cursor.char_range())
        .map(|range| {
            let (a, b) = (range.primary.index.min(length), range.secondary.index.min(length));
            (a.min(b), a.max(b))
        })
        .unwrap_or((length, length));
    let has_selection = end > start;
    let byte = |text: &str, index: usize| {
        text.char_indices()
            .nth(index)
            .map_or(text.len(), |(byte, _)| byte)
    };
    let (from, to) = if has_selection { (start, end) } else { (0, length) };
    let copied = text[byte(text, from)..byte(text, to)].to_owned();
    let pasted: String = ui
        .data(|data| data.get_temp::<String>(input_id.with("clipboard")))
        .unwrap_or_default()
        .replace(['\r', '\n'], " ");
    let set_clipboard = |value: &str| crate::desktop::clipboard::set_text(value, "find-bar");
    // Replace characters `from..to` with `with`, and put the caret after it.
    let mut replace = |from: usize, to: usize, with: &str| {
        let range = byte(text, from)..byte(text, to);
        text.replace_range(range, with);
        let caret = from + with.chars().count();
        select(&ctx, input_id, caret, caret);
    };
    if ui
        .add_enabled(!copied.is_empty(), egui::Button::new("Cut"))
        .clicked()
    {
        set_clipboard(&copied);
        replace(from, to, "");
        ui.close();
    }
    if ui
        .add_enabled(!copied.is_empty(), egui::Button::new("Copy"))
        .clicked()
    {
        set_clipboard(&copied);
        ui.close();
    }
    if ui
        .add_enabled(!pasted.is_empty(), egui::Button::new("Paste"))
        .clicked()
    {
        replace(start, end, &pasted);
        ui.close();
    }
    ui.separator();
    if ui
        .add_enabled(length > 0, egui::Button::new("Select all"))
        .clicked()
    {
        select(&ctx, input_id, 0, length);
        ui.close();
    }
}

/// Focus the text field `input_id` and select characters `start..end`.
fn select(ctx: &egui::Context, input_id: Id, start: usize, end: usize) {
    if let Some(mut state) = TextEditState::load(ctx, input_id) {
        state
            .cursor
            .set_char_range(Some(CCursorRange::two(CCursor::new(start), CCursor::new(end))));
        state.store(ctx, input_id);
    }
    ctx.memory_mut(|memory| memory.request_focus(input_id));
}
