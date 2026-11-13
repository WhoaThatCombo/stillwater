/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Stillwater: the curve that animates a mouse-wheel notch.
//!
//! A notch doesn't jump: the scroll position eases to the notch's target over
//! [`NOTCH_DURATION_MS`] with a cubic ease-out (fast start, so the first frame already moves
//! about a tenth of the way, then a gentle stop). A notch that arrives while an animation runs
//! extends the target and restarts the curve from the current position *and velocity*, so fast
//! notches chain into one continuous motion instead of stopping and starting again.
//!
//! The curve per axis is a cubic Hermite segment from the current position `base` (velocity
//! `v0`) to the target (velocity 0). With `v0 = 3R/T` (R the remaining distance, T the duration)
//! it is exactly the ease-out cubic `R(1 - (1 - s)^3)`. A Hermite segment ending at rest is
//! monotonic as long as `0 <= v0 <= 3R/T`, so:
//! - a retarget never starts slower than that ease-out (each notch visibly pushes);
//! - when the motion is already faster, the velocity is kept and the duration shortened
//!   (`T = 3R/v0`) instead, so the curve neither jerks back nor overshoots.
//!
//! Positions are emitted as whole pixels: each step is `round(position) - applied`, and the
//! last one is `target - applied`, so the animation ends exactly at the target.
//!
//! This module has no dependencies outside `std`, so its tests run on their own:
//! `rustc --edition 2024 --test components/paint/smooth_scroll.rs -o smooth_scroll_tests.exe`.

use std::time::{Duration, Instant};

/// How long one notch's animation lasts.
pub(crate) const NOTCH_DURATION_MS: f32 = 130.0;

/// A retarget never makes the curve shorter than this.
const MIN_DURATION_MS: f32 = 30.0;

/// How far into the curve a new animation starts. The first step is applied as soon as the
/// notch arrives (and shows in the next frame), rather than one frame later at t = 0, where
/// the curve hasn't moved yet.
pub(crate) const FIRST_STEP_MS: f32 = 8.0;

#[derive(Clone, Copy, Debug, Default)]
struct Axis {
    /// The sum of all deltas of this animation (whole pixels).
    target: f32,
    /// Whole pixels already emitted.
    applied: f32,
    /// Where the current curve segment starts (relative to the animation's start).
    base: f32,
    /// Velocity at the start of the segment, in pixels per millisecond.
    v0: f32,
    /// Length of the segment in milliseconds.
    duration: f32,
}

impl Axis {
    fn remaining_distance(&self) -> f32 {
        self.target - self.base
    }

    /// Position (relative to the animation's start) and velocity `t` ms into the segment.
    fn sample(&self, t: f32) -> (f32, f32) {
        let distance = self.remaining_distance();
        if self.duration <= 0.0 || t >= self.duration {
            return (self.target, 0.0);
        }
        let t = t.max(0.0);
        let s = t / self.duration;
        let h10 = s * s * s - 2.0 * s * s + s;
        let h01 = -2.0 * s * s * s + 3.0 * s * s;
        let position = self.base + h10 * self.duration * self.v0 + h01 * distance;
        let dh10 = 3.0 * s * s - 4.0 * s + 1.0;
        let dh01 = -6.0 * s * s + 6.0 * s;
        let velocity = dh10 * self.v0 + dh01 * distance / self.duration;
        (position, velocity)
    }

    /// Start a new segment at `position`/`velocity` towards `self.target`.
    fn restart(&mut self, position: f32, velocity: f32) {
        self.base = position;
        let distance = self.remaining_distance();
        if distance == 0.0 {
            self.v0 = 0.0;
            self.duration = 0.0;
            return;
        }
        let ease_out_velocity = 3.0 * distance / NOTCH_DURATION_MS;
        // Velocity along the direction we now have to go (0 if we were going the other way).
        let along = (velocity * distance.signum()).max(0.0);
        if along <= ease_out_velocity.abs() {
            self.v0 = ease_out_velocity;
            self.duration = NOTCH_DURATION_MS;
        } else {
            let duration = (3.0 * distance.abs() / along).max(MIN_DURATION_MS);
            self.duration = duration;
            self.v0 = 3.0 * distance / duration;
        }
    }

    fn is_done(&self) -> bool {
        self.applied == self.target
    }
}

/// One running wheel animation (both axes).
#[derive(Clone, Copy, Debug)]
pub(crate) struct SmoothScrollAnimation {
    /// When the current segment started.
    segment_start: Instant,
    x: Axis,
    y: Axis,
}

impl SmoothScrollAnimation {
    /// Start animating a scroll by `delta` (whole pixels per axis) at `now`.
    pub(crate) fn new(delta: (f32, f32), now: Instant) -> Self {
        let mut animation = Self {
            segment_start: now
                .checked_sub(Duration::from_secs_f32(FIRST_STEP_MS / 1000.0))
                .unwrap_or(now),
            x: Axis::default(),
            y: Axis::default(),
        };
        animation.x.target = delta.0.round();
        animation.y.target = delta.1.round();
        animation.x.restart(0.0, 0.0);
        animation.y.restart(0.0, 0.0);
        animation
    }

    fn elapsed_ms(&self, now: Instant) -> f32 {
        now.saturating_duration_since(self.segment_start)
            .as_secs_f32() *
            1000.0
    }

    /// A further notch: extend the target by `delta` and restart the curve from where the
    /// animation is at `now`, keeping its velocity.
    pub(crate) fn add_delta(&mut self, delta: (f32, f32), now: Instant) {
        let t = self.elapsed_ms(now);
        for (axis, delta) in [(&mut self.x, delta.0), (&mut self.y, delta.1)] {
            let (position, velocity) = axis.sample(t);
            axis.target += delta.round();
            axis.restart(position, velocity);
        }
        self.segment_start = now;
    }

    /// The whole-pixel step to apply at `now`, and whether the animation continues after it.
    pub(crate) fn step(&mut self, now: Instant) -> ((f32, f32), bool) {
        let t = self.elapsed_ms(now);
        let mut step = (0.0, 0.0);
        for (axis, out) in [(&mut self.x, &mut step.0), (&mut self.y, &mut step.1)] {
            let (position, _) = axis.sample(t);
            let position = if t >= axis.duration {
                axis.target
            } else {
                position.round()
            };
            *out = position - axis.applied;
            axis.applied = position;
        }
        (step, !(self.x.is_done() && self.y.is_done()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(start: Instant, ms: f32) -> Instant {
        start + Duration::from_secs_f32(ms / 1000.0)
    }

    /// Run an animation, adding `notches` (time ms, delta) and sampling every `frame` ms.
    /// Returns (time, cumulative position) samples.
    fn run(notches: &[(f32, f32)], frame: f32) -> Vec<(f32, f32)> {
        let start = Instant::now();
        let mut animation: Option<SmoothScrollAnimation> = None;
        let mut position = 0.0;
        let mut samples = vec![];
        let mut next_notch = 0;
        let mut t = 0.0;
        loop {
            while next_notch < notches.len() && notches[next_notch].0 <= t {
                let delta = (0.0, notches[next_notch].1);
                match animation.as_mut() {
                    Some(animation) => animation.add_delta(delta, ms(start, t)),
                    None => {
                        let mut new = SmoothScrollAnimation::new(delta, ms(start, t));
                        // The first step is applied as the notch arrives.
                        let (step, _) = new.step(ms(start, t));
                        position += step.1;
                        animation = Some(new);
                    },
                }
                next_notch += 1;
            }
            t += frame;
            if let Some(current) = animation.as_mut() {
                let (step, more) = current.step(ms(start, t));
                position += step.1;
                if !more {
                    animation = None;
                }
            }
            samples.push((t, position));
            if animation.is_none() && next_notch == notches.len() {
                break;
            }
            assert!(t < 5000.0, "animation never ended");
        }
        samples
    }

    #[test]
    fn one_notch_eases_to_exact_target() {
        let samples = run(&[(0.0, 100.0)], 6.1);
        let positions: Vec<f32> = samples.iter().map(|s| s.1).collect();
        assert_eq!(*positions.last().unwrap(), 100.0);
        assert!(positions.windows(2).all(|w| w[1] >= w[0]), "{positions:?}");
        // Moves right away: about 18 % by the first frame (8 ms + 6.1 ms into the curve).
        assert!(positions[0] >= 10.0, "{positions:?}");
        // Done within the duration (plus one frame).
        let end = samples.iter().find(|s| s.1 == 100.0).unwrap().0;
        assert!(end <= NOTCH_DURATION_MS + 6.1, "ended at {end}");
        // Ease-out: the first half of the time covers most of the distance.
        let half = samples
            .iter()
            .find(|s| s.0 >= NOTCH_DURATION_MS / 2.0)
            .unwrap()
            .1;
        assert!(half >= 80.0, "{half}");
    }

    #[test]
    fn negative_notch() {
        let samples = run(&[(0.0, -76.0)], 16.7);
        let positions: Vec<f32> = samples.iter().map(|s| s.1).collect();
        assert_eq!(*positions.last().unwrap(), -76.0);
        assert!(positions.windows(2).all(|w| w[1] <= w[0]), "{positions:?}");
    }

    #[test]
    fn rapid_notches_chain_without_stalling() {
        // Five notches 40 ms apart (a fast flick of the wheel).
        let notches: Vec<(f32, f32)> = (0..5).map(|i| (i as f32 * 40.0, 76.0)).collect();
        let samples = run(&notches, 6.1);
        let positions: Vec<f32> = samples.iter().map(|s| s.1).collect();
        assert_eq!(*positions.last().unwrap(), 380.0);
        assert!(positions.windows(2).all(|w| w[1] >= w[0]), "{positions:?}");
        // No stall: while notches keep coming (until the last one at 160 ms), every frame
        // moves.
        let moving: Vec<_> = samples
            .windows(2)
            .filter(|w| w[1].0 <= 170.0)
            .map(|w| w[1].1 - w[0].1)
            .collect();
        assert!(moving.iter().all(|d| *d >= 1.0), "{moving:?}");
        // Ends shortly after the last notch's own duration.
        let end = samples.iter().find(|s| s.1 == 380.0).unwrap().0;
        assert!(end <= 160.0 + NOTCH_DURATION_MS + 6.1, "ended at {end}");
    }

    #[test]
    fn retarget_keeps_velocity() {
        let start = Instant::now();
        let mut animation = SmoothScrollAnimation::new((0.0, 76.0), start);
        let t = 20.0;
        let before = animation.y.sample(animation.elapsed_ms(ms(start, t))).1;
        animation.add_delta((0.0, 76.0), ms(start, t));
        let after = animation.y.sample(0.0).1;
        // Never slower than before the new notch; a notch may push it faster.
        assert!(after >= before - 1e-4, "before {before}, after {after}");
    }

    #[test]
    fn fast_motion_shortens_rather_than_overshoots() {
        let start = Instant::now();
        let mut animation = SmoothScrollAnimation::new((0.0, 1000.0), start);
        let mut position = 0.0;
        let mut previous = 0.0;
        let mut t = 0.0;
        let mut added = false;
        loop {
            t += 1.0;
            // Late in the curve: still moving faster than a fresh 1 px notch would start,
            // with only a few pixels left.
            if t >= 100.0 && !added {
                animation.add_delta((0.0, 1.0), ms(start, t));
                added = true;
            }
            let (step, more) = animation.step(ms(start, t));
            position += step.1;
            assert!(position >= previous, "went back at {t}");
            assert!(position <= 1001.0, "overshot to {position}");
            previous = position;
            if !more {
                break;
            }
        }
        assert_eq!(position, 1001.0);
    }

    #[test]
    fn reversal() {
        let samples = run(&[(0.0, 300.0), (30.0, -76.0)], 6.1);
        assert_eq!(samples.last().unwrap().1, 224.0);
    }

    #[test]
    fn frame_rate_independent_end() {
        for frame in [6.1, 8.3, 16.7, 33.3] {
            let notches: Vec<(f32, f32)> = (0..5).map(|i| (i as f32 * 35.0, 76.0)).collect();
            let samples = run(&notches, frame);
            assert_eq!(samples.last().unwrap().1, 380.0, "frame {frame}");
        }
    }

    #[test]
    fn two_axes() {
        let start = Instant::now();
        let mut animation = SmoothScrollAnimation::new((50.0, -76.0), start);
        let (mut x, mut y) = (0.0, 0.0);
        let mut t = 0.0;
        loop {
            t += 6.1;
            let (step, more) = animation.step(ms(start, t));
            x += step.0;
            y += step.1;
            if !more {
                break;
            }
        }
        assert_eq!((x, y), (50.0, -76.0));
    }
}
