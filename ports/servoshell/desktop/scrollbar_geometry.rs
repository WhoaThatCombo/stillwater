/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! The geometry of the page's overlay scrollbars (see `scrollbar.rs`), kept free of egui and
//! Servo so it can be unit-tested on its own (`rustc --edition 2024 --test` this file).
//!
//! Lengths along the scrollbar are in window (device-independent) pixels; scroll offsets and
//! sizes are in page pixels, as `WebView::root_scroll_state` gives them. Only their ratios
//! meet, so page zoom and the display scale cancel out.

/// Axis indices into `[x, y]` arrays.
pub(crate) const X: usize = 0;
pub(crate) const Y: usize = 1;

/// The bar's thickness while idle and while the pointer is near it.
pub(crate) const THIN: f32 = 4.0;
pub(crate) const WIDE: f32 = 10.0;
/// Gap between the bar and the edge of the page.
pub(crate) const EDGE_GAP: f32 = 2.0;
/// How close (from the page's edge) the pointer must be for the bar to widen.
pub(crate) const HOVER_ZONE: f32 = 16.0;
/// Presses this close to the page's edge, along the track, go to the scrollbar.
pub(crate) const PRESS_ZONE: f32 = WIDE + EDGE_GAP + 2.0;
/// The thumb never gets shorter than this (or than the track).
pub(crate) const MIN_THUMB: f32 = 32.0;
/// A click on the track scrolls by this fraction of the viewport.
pub(crate) const PAGE_FRACTION: f32 = 0.875;
/// Track ends are kept clear of the page's rounded corners.
pub(crate) const END_INSET: f32 = 6.0;

/// A rectangle as `[min_x, min_y, max_x, max_y]`.
pub(crate) type Rect = [f32; 4];

pub(crate) fn contains(rect: Rect, point: [f32; 2]) -> bool {
    point[0] >= rect[0] && point[0] < rect[2] && point[1] >= rect[1] && point[1] < rect[3]
}

/// Whether there is anything to scroll on an axis.
pub(crate) fn scrollable(viewport: f32, content: f32) -> bool {
    viewport > 0.0 && content - viewport >= 1.0
}

/// The thumb's `(start, length)` along a track `track` long, for a viewport of `viewport`
/// over content `content` long scrolled to `offset`. `None` if there's nothing to scroll.
pub(crate) fn thumb(track: f32, viewport: f32, content: f32, offset: f32) -> Option<(f32, f32)> {
    if track <= 0.0 || !scrollable(viewport, content) {
        return None;
    }
    let length = (track * viewport / content).max(MIN_THUMB).min(track);
    let max_offset = content - viewport;
    let start = (track - length) * (offset / max_offset).clamp(0.0, 1.0);
    Some((start, length))
}

/// The scroll offset that puts the thumb's start at `start` (track-relative).
pub(crate) fn offset_for_thumb_start(track: f32, viewport: f32, content: f32, start: f32) -> f32 {
    let Some((_, length)) = thumb(track, viewport, content, 0.0) else {
        return 0.0;
    };
    let range = track - length;
    if range <= 0.0 {
        return 0.0;
    }
    (start / range).clamp(0.0, 1.0) * (content - viewport)
}

/// Dragging: the offset for the pointer at `pointer` (track-relative) when the thumb was
/// grabbed `grab` pixels from its start.
pub(crate) fn drag_offset(track: f32, viewport: f32, content: f32, pointer: f32, grab: f32) -> f32 {
    offset_for_thumb_start(track, viewport, content, pointer - grab)
}

/// A press at `pointer` (track-relative): `0` on the thumb, `-1` before it (page up/left),
/// `1` after it (page down/right).
pub(crate) fn track_click(thumb_start: f32, thumb_length: f32, pointer: f32) -> i8 {
    if pointer < thumb_start {
        -1
    } else if pointer >= thumb_start + thumb_length {
        1
    } else {
        0
    }
}

/// The track of the bar for `axis` over a page drawn in `view`, `width` thick. `both` is
/// whether the other axis has a bar too (each then leaves the corner free).
pub(crate) fn track_rect(view: Rect, axis: usize, both: bool, width: f32) -> Rect {
    let corner = if both { WIDE + EDGE_GAP } else { 0.0 };
    if axis == Y {
        [
            view[2] - EDGE_GAP - width,
            view[1] + END_INSET,
            view[2] - EDGE_GAP,
            (view[3] - END_INSET - corner).max(view[1] + END_INSET),
        ]
    } else {
        [
            view[0] + END_INSET,
            view[3] - EDGE_GAP - width,
            (view[2] - END_INSET - corner).max(view[0] + END_INSET),
            view[3] - EDGE_GAP,
        ]
    }
}

/// The track's length along its axis.
pub(crate) fn track_length(track: Rect, axis: usize) -> f32 {
    track[axis + 2] - track[axis]
}

/// Where the pointer must be for the bar of `axis` to widen.
pub(crate) fn hover_zone(view: Rect, axis: usize) -> Rect {
    if axis == Y {
        [view[2] - HOVER_ZONE, view[1], view[2], view[3]]
    } else {
        [view[0], view[3] - HOVER_ZONE, view[2], view[3]]
    }
}

/// Where a press goes to the bar of `axis` rather than to the page.
pub(crate) fn press_zone(view: Rect, axis: usize, both: bool) -> Rect {
    let track = track_rect(view, axis, both, WIDE);
    if axis == Y {
        [view[2] - PRESS_ZONE, track[1], view[2], track[3]]
    } else {
        [track[0], view[3] - PRESS_ZONE, track[2], view[3]]
    }
}

/// The thumb's rectangle on `track` (for a bar `axis`), given its `(start, length)`.
pub(crate) fn thumb_rect(track: Rect, axis: usize, (start, length): (f32, f32)) -> Rect {
    let mut rect = track;
    rect[axis] = track[axis] + start;
    rect[axis + 2] = track[axis] + start + length;
    rect
}

/// How opaque an auto-hiding bar is `since` seconds after the last scroll or hover:
/// fully shown for `SHOW_FOR`, then fading out over `FADE_FOR`.
pub(crate) const SHOW_FOR: f32 = 1.5;
pub(crate) const FADE_FOR: f32 = 0.25;
pub(crate) fn auto_hide_opacity(since: f32) -> f32 {
    if since <= SHOW_FOR {
        1.0
    } else {
        (1.0 - (since - SHOW_FOR) / FADE_FOR).clamp(0.0, 1.0)
    }
}

/// Whether a page whose background is `rgb` (0–1) is dark, so the thumb should be light.
pub(crate) fn is_dark(rgb: [f32; 3]) -> bool {
    0.2126 * rgb[0] + 0.7152 * rgb[1] + 0.0722 * rgb[2] < 0.5
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 0.01
    }

    #[test]
    fn nothing_to_scroll_has_no_thumb() {
        assert_eq!(thumb(500.0, 800.0, 800.0, 0.0), None);
        assert_eq!(thumb(500.0, 800.0, 800.5, 0.0), None);
        assert_eq!(thumb(500.0, 0.0, 800.0, 0.0), None);
        assert_eq!(thumb(0.0, 800.0, 4000.0, 0.0), None);
    }

    #[test]
    fn thumb_is_proportional_and_positioned() {
        // A viewport of 1/5 of the content: the thumb is 1/5 of the track.
        let (start, length) = thumb(500.0, 800.0, 4000.0, 0.0).unwrap();
        assert!(close(start, 0.0) && close(length, 100.0));
        // Half way: the thumb is centred in the travel.
        let (start, _) = thumb(500.0, 800.0, 4000.0, 1600.0).unwrap();
        assert!(close(start, 200.0));
        // At the end: the thumb touches the track's end.
        let (start, length) = thumb(500.0, 800.0, 4000.0, 3200.0).unwrap();
        assert!(close(start + length, 500.0));
        // Overscroll (rubber-banding, stale offsets) stays on the track.
        let (start, _) = thumb(500.0, 800.0, 4000.0, 9999.0).unwrap();
        assert!(close(start, 400.0));
        let (start, _) = thumb(500.0, 800.0, 4000.0, -50.0).unwrap();
        assert!(close(start, 0.0));
    }

    #[test]
    fn thumb_has_a_minimum_length() {
        let (_, length) = thumb(500.0, 800.0, 400_000.0, 0.0).unwrap();
        assert!(close(length, MIN_THUMB));
        // ...but never more than the track.
        let (start, length) = thumb(20.0, 800.0, 400_000.0, 1000.0).unwrap();
        assert!(close(length, 20.0) && close(start, 0.0));
    }

    #[test]
    fn dragging_maps_back_to_offsets() {
        let (track, viewport, content) = (500.0, 800.0, 4000.0);
        for offset in [0.0, 1.0, 640.0, 1600.0, 3199.0, 3200.0] {
            let (start, _) = thumb(track, viewport, content, offset).unwrap();
            assert!(close(offset_for_thumb_start(track, viewport, content, start), offset));
        }
        // Grabbed 30 px into the thumb at the top, pointer moved down 100 px: the thumb's
        // start is at 100 of 400 px of travel, so a quarter of the way.
        assert!(close(drag_offset(track, viewport, content, 130.0, 30.0), 800.0));
        // Dragging past either end clamps.
        assert!(close(drag_offset(track, viewport, content, -500.0, 30.0), 0.0));
        assert!(close(drag_offset(track, viewport, content, 5000.0, 30.0), 3200.0));
        // With the minimum thumb length the mapping still covers the whole page.
        let content = 400_000.0;
        assert!(close(drag_offset(track, viewport, content, 500.0 - MIN_THUMB, 0.0), content - viewport));
    }

    #[test]
    fn track_clicks_page_towards_the_pointer() {
        assert_eq!(track_click(100.0, 50.0, 10.0), -1);
        assert_eq!(track_click(100.0, 50.0, 100.0), 0);
        assert_eq!(track_click(100.0, 50.0, 149.0), 0);
        assert_eq!(track_click(100.0, 50.0, 150.0), 1);
    }

    #[test]
    fn tracks_sit_at_the_right_and_bottom_edges() {
        let view = [100.0, 50.0, 900.0, 650.0];
        let vertical = track_rect(view, Y, false, THIN);
        assert_eq!(vertical, [894.0, 56.0, 898.0, 644.0]);
        // Widening keeps the length (drag mapping doesn't jump) and grows inward.
        let wide = track_rect(view, Y, false, WIDE);
        assert_eq!(track_length(wide, Y), track_length(vertical, Y));
        assert_eq!(wide[2], vertical[2]);
        assert!(wide[0] < vertical[0]);
        let horizontal = track_rect(view, X, false, THIN);
        assert_eq!(horizontal, [106.0, 644.0, 894.0, 648.0]);
        // With both bars, each leaves the corner free.
        let vertical_both = track_rect(view, Y, true, THIN);
        let horizontal_both = track_rect(view, X, true, THIN);
        assert!(vertical_both[3] <= view[3] - WIDE - EDGE_GAP);
        assert!(horizontal_both[2] <= view[2] - WIDE - EDGE_GAP);
    }

    #[test]
    fn zones() {
        let view = [100.0, 50.0, 900.0, 650.0];
        assert!(contains(hover_zone(view, Y), [885.0, 300.0]));
        assert!(!contains(hover_zone(view, Y), [883.0, 300.0]));
        assert!(contains(press_zone(view, Y, false), [887.0, 300.0]));
        assert!(!contains(press_zone(view, Y, false), [885.0, 300.0]));
        assert!(!contains(press_zone(view, Y, false), [899.0, 51.0]));
        assert!(contains(press_zone(view, X, false), [500.0, 640.0]));
        // Outside the page, nothing.
        assert!(!contains(hover_zone(view, Y), [900.0, 300.0]));
    }

    #[test]
    fn thumb_rect_lies_on_the_track() {
        let track = [894.0, 56.0, 898.0, 644.0];
        assert_eq!(thumb_rect(track, Y, (100.0, 50.0)), [894.0, 156.0, 898.0, 206.0]);
        let track = [106.0, 644.0, 894.0, 648.0];
        assert_eq!(thumb_rect(track, X, (10.0, 40.0)), [116.0, 644.0, 156.0, 648.0]);
    }

    #[test]
    fn fades_after_a_pause() {
        assert_eq!(auto_hide_opacity(0.0), 1.0);
        assert_eq!(auto_hide_opacity(1.5), 1.0);
        assert!(close(auto_hide_opacity(1.625), 0.5));
        assert_eq!(auto_hide_opacity(1.75), 0.0);
        assert_eq!(auto_hide_opacity(100.0), 0.0);
    }

    #[test]
    fn contrast() {
        assert!(is_dark([0.0, 0.0, 0.0]));
        assert!(is_dark([0.12, 0.12, 0.14]));
        assert!(!is_dark([1.0, 1.0, 1.0]));
        assert!(!is_dark([0.9, 0.9, 0.85]));
    }
}
