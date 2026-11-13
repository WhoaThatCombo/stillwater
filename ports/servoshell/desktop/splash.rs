/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! The startup animation: Stillwater's two waves roll in and come to rest as the mark, one
//! ring spreads from them, the name appears, and the whole thing fades to the browser.
//!
//! It covers the window for about three seconds while the sidebar page, the restored tabs
//! and the ad-block lists load behind it, which on a slow disk is most of what would
//! otherwise be watched arriving piece by piece. It is a few dozen line segments on egui's
//! top layer: no textures, no sound, nothing loaded from disk.

use std::f32::consts::TAU;
use std::time::Instant;

use egui::{Align2, Color32, FontId, Id, LayerId, Order, Pos2, Rect, Shape, Stroke, pos2, vec2};

/// The waves have come to rest by now (seconds on the animation's own clock).
const SETTLED: f32 = 1.7;
/// The fade to the browser starts here, if the sidebar is ready.
const HOLD_UNTIL: f32 = 2.4;
const FADE: f32 = 0.4;
/// The animation's clock never jumps further than this in one frame, so a stall while the
/// disk is busy slows the animation instead of skipping it.
const LONGEST_STEP: f32 = 0.05;
/// However slow start-up is, the window is uncovered after this long (real seconds).
const GIVE_UP: f32 = 8.0;
/// The first second of start-up is when the window is busiest (the session's tabs and the
/// sidebar are being created) and frames come in fits. The animation's fastest motion is its
/// opening, so it waits on the plain background until this many frames in a row have arrived
/// on time, or `LONGEST_WAIT` has passed, and only then starts to move.
const STEADY_FRAMES: u8 = 4;
const ON_TIME: f32 = 0.034;
const LONGEST_WAIT: f32 = 1.2;

/// The mark's upper wave, as drawn in the icon.
const WAVE: Color32 = Color32::from_rgb(96, 108, 255);

pub(crate) struct Splash {
    /// Seconds of animation shown so far.
    clock: f32,
    /// When the fade to the browser began, on `clock`.
    fade_from: Option<f32>,
    started: Option<Instant>,
    last_frame: Option<Instant>,
    /// Frames in a row that arrived on time, until the animation starts moving.
    steady: u8,
    moving: bool,
}

impl Splash {
    pub(crate) fn new() -> Self {
        Self {
            clock: 0.0,
            fade_from: None,
            started: None,
            last_frame: None,
            steady: 0,
            moving: false,
        }
    }

    /// Whether to play it: on by default, off for automated runs (WebDriver), which would
    /// otherwise screenshot and time the animation instead of the page.
    /// `STILLWATER_SPLASH=1` forces it for tests; `STILLWATER_NO_SPLASH` turns it off.
    pub(crate) fn wanted(setting: bool, automated: bool) -> bool {
        if std::env::var_os("STILLWATER_SPLASH").is_some() {
            return true;
        }
        setting && !automated && std::env::var_os("STILLWATER_NO_SPLASH").is_none()
    }

    /// Draw one frame over `rect`. `ready` is whether the browser behind is fit to be seen.
    /// Returns `false` once the animation has finished.
    pub(crate) fn paint(
        &mut self,
        ctx: &egui::Context,
        rect: Rect,
        background: Color32,
        glass: bool,
        ready: bool,
    ) -> bool {
        let now = Instant::now();
        let started = *self.started.get_or_insert(now);
        if let Some(last) = self.last_frame {
            let step = (now - last).as_secs_f32();
            if self.moving {
                self.clock += step.min(LONGEST_STEP);
            } else {
                self.steady = if step <= ON_TIME { self.steady + 1 } else { 0 };
                self.moving = self.steady >= STEADY_FRAMES ||
                    (now - started).as_secs_f32() > LONGEST_WAIT;
            }
        }
        self.last_frame = Some(now);
        let t = self.clock;

        // A click or a key means "I'm here, let me in".
        let impatient = ctx.input(|input| input.pointer.any_pressed() || !input.keys_down.is_empty());
        let overdue = (now - started).as_secs_f32() > GIVE_UP;
        if self.fade_from.is_none() &&
            ((t >= HOLD_UNTIL && ready) || (impatient && t > 0.25) || overdue)
        {
            self.fade_from = Some(t);
        }
        let cover = match self.fade_from {
            Some(from) => 1.0 - smooth((t - from) / FADE),
            None => 1.0,
        };
        if cover <= 0.0 {
            return false;
        }

        let [r, g, b, _] = background.to_srgba_unmultiplied();
        let dark = luminance(Color32::from_rgb(r, g, b)) < 0.5;
        let ink = if dark { Color32::from_rgb(236, 238, 245) } else { Color32::from_rgb(28, 30, 38) };
        let under_wave = if dark { Color32::from_rgb(120, 130, 150) } else { Color32::from_rgb(150, 158, 176) };

        let painter = ctx.layer_painter(LayerId::new(Order::Debug, Id::new("startup-animation")));
        if glass {
            // Glass: hide what is behind by replacing it with the translucent tint (blending
            // the tint over it would let the page show through).
            painter.add(crate::desktop::glass::fill(rect, background, cover, None));
        } else {
            painter.rect_filled(rect, 0.0, fade(background, cover));
        }

        // Sized from the window, so it sits right in a small window and a maximised one.
        let unit = (rect.width().min(rect.height()) / 6.5).clamp(40.0, 120.0);
        let centre = pos2(rect.center().x, rect.center().y - unit * 0.35);

        // 1 while the water is still moving, 0 once it is at rest.
        let unrest = 1.0 - ease_out((t / SETTLED).min(1.0));
        // The mark leaves in the first half of the fade, so it is gone before the page
        // behind (which may show the same mark) comes through.
        let leaving = match self.fade_from {
            Some(from) => 1.0 - smooth((t - from) / (FADE * 0.5)),
            None => 1.0,
        };
        let appear = smooth(t / 0.4) * leaving;

        // The ring: one slow circle spreading from the mark as it settles.
        let ring = ((t - (SETTLED - 0.45)) / 1.5).clamp(0.0, 1.0);
        if ring > 0.0 && ring < 1.0 {
            let radius = unit * (1.0 + 2.4 * ease_out(ring));
            let strength = (1.0 - ring).powi(2) * 0.35 * leaving;
            painter.circle_stroke(centre, radius, Stroke::new(1.5, fade(WAVE, strength)));
        }

        // The upper wave: one full swell, rolling in from the left and slowing to a stop.
        wave(
            &painter,
            centre + vec2(0.0, -unit * 0.16),
            unit * 1.30 * (1.0 + 0.55 * unrest),
            unit * 0.20 * (1.0 + 0.35 * unrest),
            -6.0 * unrest,
            unit * 0.17,
            fade(WAVE, appear),
        );
        // The lower one: shorter, fainter, a little behind, as in the icon.
        wave(
            &painter,
            centre + vec2(unit * 0.02, unit * 0.36),
            unit * 0.74 * (1.0 + 0.9 * unrest),
            unit * 0.085 * (1.0 + 0.6 * unrest),
            -4.2 * unrest - 0.6 * unrest * unrest,
            unit * 0.065,
            fade(under_wave, appear * 0.9),
        );

        // The name rises into place once the water is nearly still.
        let named = smooth((t - 1.0) / 0.7);
        if named > 0.0 {
            painter.text(
                centre + vec2(0.0, unit * 1.12 + (1.0 - named) * 7.0),
                Align2::CENTER_CENTER,
                "Stillwater",
                FontId::proportional((unit * 0.34).clamp(15.0, 26.0)),
                fade(ink, named * 0.92 * leaving),
            );
        }

        ctx.request_repaint();
        true
    }
}

/// A sine wave `width` wide centred on `centre`, drawn as a round-ended line.
fn wave(
    painter: &egui::Painter,
    centre: Pos2,
    width: f32,
    amplitude: f32,
    phase: f32,
    thickness: f32,
    color: Color32,
) {
    const SEGMENTS: usize = 40;
    let points: Vec<Pos2> = (0..=SEGMENTS)
        .map(|i| {
            let x = i as f32 / SEGMENTS as f32;
            // Just short of a full period at each end, like the icon's wave.
            let angle = TAU * (0.04 + x * 0.92) + phase;
            pos2(centre.x + (x - 0.5) * width, centre.y - amplitude * angle.sin())
        })
        .collect();
    let (first, last) = (points[0], points[SEGMENTS]);
    painter.add(Shape::line(points, Stroke::new(thickness, color)));
    painter.circle_filled(first, thickness / 2.0, color);
    painter.circle_filled(last, thickness / 2.0, color);
}

fn fade(color: Color32, opacity: f32) -> Color32 {
    color.gamma_multiply(opacity.clamp(0.0, 1.0))
}

fn smooth(x: f32) -> f32 {
    let x = x.clamp(0.0, 1.0);
    x * x * (3.0 - 2.0 * x)
}

fn ease_out(x: f32) -> f32 {
    1.0 - (1.0 - x.clamp(0.0, 1.0)).powi(3)
}

fn luminance(color: Color32) -> f32 {
    (0.299 * color.r() as f32 + 0.587 * color.g() as f32 + 0.114 * color.b() as f32) / 255.0
}
