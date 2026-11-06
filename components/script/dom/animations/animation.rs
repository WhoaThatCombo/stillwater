/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::Cell;
use std::time::Duration;

use dom_struct::dom_struct;
use js::context::JSContext;
use js::rust::HandleObject;
use script_bindings::cell::DomRefCell;
use script_bindings::codegen::GenericBindings::DocumentBinding::DocumentMethods;
use script_bindings::codegen::GenericBindings::WindowBinding::WindowMethods;
use script_bindings::inheritance::Castable;
use script_bindings::reflector::reflect_dom_object_with_proto;
use script_bindings::root::DomRoot;
use servo_config::pref;
use stylo_atoms::Atom;
use timers::{TimerEventRequest, TimerId};

use crate::dom::animationeffect::{AnimationEffect, EffectPhase};
use crate::dom::animations::keyframeeffect::apply_script_animations_to;
use crate::dom::bindings::codegen::Bindings::AnimationBinding::{
    AnimationMethods, AnimationPlayState, AnimationReplaceState,
};
use crate::dom::bindings::codegen::Bindings::AnimationTimelineBinding::AnimationTimelineMethods;
use crate::dom::bindings::error::{Error, ErrorResult};
use crate::dom::bindings::num::Finite;
use crate::dom::bindings::refcounted::Trusted;
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::{Dom, MutNullableDom};
use crate::dom::bindings::str::DOMString;
use crate::dom::event::Event;
use crate::dom::eventtarget::EventTarget;
use crate::dom::globalscope::GlobalScope;
use crate::dom::promise::{Promise, RootedPromise, TracedPromise};
use crate::dom::types::{
    AnimationPlaybackEvent, AnimationTimeline, DocumentTimeline, KeyframeEffect,
};
use crate::dom::window::Window;
use crate::event_loop::script_thread::with_script_thread;
use crate::realms::enter_auto_realm;
use crate::runtime::job_queue::MicrotaskRunnable;

/// <https://drafts.csswg.org/web-animations-1/#animation>
///
/// This implements the timing model of an animation (play state, start/hold times, playback
/// rate, the ready and finished promises and the finish/cancel events). Times are in
/// milliseconds of the animation's timeline. The animation wakes itself up with a timer when it
/// is due to finish, so that its promises and events don't depend on rendering updates.
#[dom_struct]
pub(crate) struct Animation {
    event_target: EventTarget,

    /// <https://drafts.csswg.org/web-animations-1/#timeline>
    timeline: MutNullableDom<AnimationTimeline>,

    /// <https://drafts.csswg.org/web-animations-1/#animation-associated-effect>
    associated_effect: MutNullableDom<AnimationEffect>,

    /// <https://drafts.csswg.org/web-animations-1/#dom-animation-id>
    id: DomRefCell<DOMString>,

    /// <https://drafts.csswg.org/web-animations-1/#animation-start-time>
    start_time: Cell<Option<f64>>,

    /// <https://drafts.csswg.org/web-animations-1/#animation-hold-time>
    hold_time: Cell<Option<f64>>,

    /// <https://drafts.csswg.org/web-animations-1/#playback-rate>
    playback_rate: Cell<f64>,

    /// <https://drafts.csswg.org/web-animations-1/#pending-playback-rate>
    pending_playback_rate: Cell<Option<f64>>,

    /// <https://drafts.csswg.org/web-animations-1/#pending-play-task>
    pending_play_task: Cell<bool>,

    /// <https://drafts.csswg.org/web-animations-1/#pending-pause-task>
    pending_pause_task: Cell<bool>,

    /// <https://drafts.csswg.org/web-animations-1/#previous-current-time>
    previous_current_time: Cell<Option<f64>>,

    /// <https://drafts.csswg.org/web-animations-1/#current-ready-promise>
    ready_promise: DomRefCell<TracedPromise>,

    /// <https://drafts.csswg.org/web-animations-1/#current-finished-promise>
    finished_promise: DomRefCell<TracedPromise>,

    /// Whether a microtask to run the finish notification steps is already queued.
    finish_notification_queued: Cell<bool>,

    /// <https://drafts.csswg.org/web-animations-1/#dom-animation-persist>
    persisted: Cell<bool>,

    /// Incremented whenever a new wake-up timer is scheduled; older timers then do nothing.
    timer_generation: Cell<u64>,

    /// The currently scheduled wake-up timer.
    #[no_trace]
    timer_id: Cell<Option<TimerId>>,
}

impl Animation {
    fn new_inherited(ready_promise: &RootedPromise, finished_promise: &RootedPromise) -> Self {
        Self {
            event_target: EventTarget::new_inherited(),
            timeline: Default::default(),
            associated_effect: Default::default(),
            id: Default::default(),
            start_time: Cell::new(None),
            hold_time: Cell::new(None),
            playback_rate: Cell::new(1.0),
            pending_playback_rate: Cell::new(None),
            pending_play_task: Cell::new(false),
            pending_pause_task: Cell::new(false),
            previous_current_time: Cell::new(None),
            ready_promise: DomRefCell::new(ready_promise.to_traced()),
            finished_promise: DomRefCell::new(finished_promise.to_traced()),
            finish_notification_queued: Cell::new(false),
            persisted: Cell::new(false),
            timer_generation: Cell::new(0),
            timer_id: Cell::new(None),
        }
    }

    fn new_with_proto(
        cx: &mut JSContext,
        global: &GlobalScope,
        proto: Option<HandleObject>,
    ) -> DomRoot<Self> {
        let ready_promise = Promise::new(cx, global);
        let finished_promise = Promise::new(cx, global);
        let animation = reflect_dom_object_with_proto(
            cx,
            Box::new(Self::new_inherited(&ready_promise, &finished_promise)),
            global,
            proto,
        );
        // > The current ready promise is initially a resolved Promise created using the
        // > procedure to create a new resolved Promise with the animation itself as its value.
        ready_promise.resolve_native(cx, &*animation);
        animation
    }

    pub(crate) fn new(cx: &mut JSContext, global: &GlobalScope) -> DomRoot<Self> {
        Self::new_with_proto(cx, global, None)
    }

    fn window(&self) -> DomRoot<Window> {
        DomRoot::downcast::<Window>(self.global()).expect("Animation is only exposed to Window")
    }

    /// Bring the animation's timeline up to date. Servo otherwise only advances document
    /// timelines when it renders a frame, and an idle page may not render for a long time.
    pub(crate) fn refresh_timeline(&self) {
        let Some(timeline) = self.timeline.get() else {
            return;
        };
        if pref!(layout_animations_test_enabled) {
            return;
        }
        if let Some(document_timeline) = timeline.downcast::<DocumentTimeline>() {
            document_timeline.update(&self.window());
        }
    }

    /// The timeline's current time in milliseconds, or `None` without an active timeline.
    fn timeline_time(&self) -> Option<f64> {
        self.timeline
            .get()
            .and_then(|timeline| timeline.GetCurrentTime())
            .map(|time| *time)
    }

    pub(crate) fn set_id(&self, id: DOMString) {
        *self.id.borrow_mut() = id;
    }

    pub(crate) fn playback_rate(&self) -> f64 {
        self.playback_rate.get()
    }

    /// <https://drafts.csswg.org/web-animations-1/#effective-playback-rate>
    fn effective_playback_rate(&self) -> f64 {
        self.pending_playback_rate
            .get()
            .unwrap_or(self.playback_rate.get())
    }

    /// <https://drafts.csswg.org/web-animations-1/#apply-any-pending-playback-rate>
    fn apply_any_pending_playback_rate(&self) {
        if let Some(rate) = self.pending_playback_rate.take() {
            self.playback_rate.set(rate);
        }
    }

    /// <https://drafts.csswg.org/web-animations-1/#associated-effect-end>
    fn associated_effect_end(&self) -> f64 {
        self.associated_effect
            .get()
            .map_or(0.0, |effect| effect.end_time())
    }

    /// <https://drafts.csswg.org/web-animations-1/#pending-play-task>
    pub(crate) fn has_pending_task(&self) -> bool {
        self.pending_play_task.get() || self.pending_pause_task.get()
    }

    /// <https://drafts.csswg.org/web-animations-1/#animation-current-time>
    pub(crate) fn current_time(&self) -> Option<f64> {
        self.calculate_current_time(false)
    }

    fn calculate_current_time(&self, ignore_hold_time: bool) -> Option<f64> {
        if !ignore_hold_time && let Some(hold_time) = self.hold_time.get() {
            return Some(hold_time);
        }
        let timeline_time = self.timeline_time()?;
        let start_time = self.start_time.get()?;
        Some((timeline_time - start_time) * self.playback_rate.get())
    }

    /// <https://drafts.csswg.org/web-animations-1/#play-states>
    pub(crate) fn play_state(&self) -> AnimationPlayState {
        let current_time = self.current_time();
        if current_time.is_none() && self.start_time.get().is_none() && !self.has_pending_task() {
            return AnimationPlayState::Idle;
        }
        if self.pending_pause_task.get() ||
            (self.start_time.get().is_none() && !self.pending_play_task.get())
        {
            return AnimationPlayState::Paused;
        }
        let rate = self.effective_playback_rate();
        if let Some(current_time) = current_time &&
            ((rate > 0.0 && current_time >= self.associated_effect_end()) ||
                (rate < 0.0 && current_time <= 0.0))
        {
            return AnimationPlayState::Finished;
        }
        AnimationPlayState::Running
    }

    fn resolve_ready_promise(&self, cx: &mut JSContext) {
        let promise = self.ready_promise.borrow().root(cx);
        if !promise.is_fulfilled() {
            promise.resolve_native(cx, self);
        }
    }

    fn create_new_ready_promise(&self, cx: &mut JSContext) {
        let promise = Promise::new(cx, &self.global());
        *self.ready_promise.borrow_mut() = promise.to_traced();
    }

    /// <https://drafts.csswg.org/web-animations-1/#animation-set-the-timeline-of-an-animation>
    fn set_the_timeline(&self, cx: &mut JSContext, timeline: Option<&AnimationTimeline>) {
        // Step 1. Let old timeline be the current timeline of animation, if any.
        // Step 2. If new timeline is the same object as old timeline, abort this procedure.
        if self.timeline.get().as_deref() == timeline {
            return;
        }
        let previous_play_state = self.play_state();

        // Step 3. Let the timeline of animation be new timeline.
        self.timeline.set(timeline);
        self.refresh_timeline();

        // Step 4. If the start time of animation is resolved, make animation's hold time
        // unresolved.
        if self.start_time.get().is_some() {
            self.hold_time.set(None);
        }

        // Keep a running animation running against its new timeline.
        if previous_play_state == AnimationPlayState::Running && self.timeline_time().is_none() {
            self.hold_time.set(self.previous_current_time.get());
            self.start_time.set(None);
        }

        // Step 5. Run the procedure to update an animation's finished state for animation with
        // the did seek flag set to false, and the synchronously notify flag set to false.
        self.update_finished_state(cx, false, false);
    }

    /// <https://drafts.csswg.org/web-animations-1/#animation-set-the-associated-effect-of-an-animation>
    fn set_the_associated_effect(&self, cx: &mut JSContext, effect: Option<&AnimationEffect>) {
        // Step 1. Let old effect be the current associated effect of animation, if any.
        let old_effect = self.associated_effect.get();

        // Step 2. If new effect is the same object as old effect, abort this procedure.
        if old_effect.as_deref() == effect {
            return;
        }

        // Step 3. If animation has a pending pause task, reschedule that task to run as soon as
        // animation is ready.
        // Step 4. If animation has a pending play task, reschedule that task to run as soon as
        // animation is ready to play new effect.
        // Note: pending tasks always run on the next wake-up timer.

        // Step 5. If new effect is not null and if new effect is the associated effect of
        // another animation, previous animation, run the procedure to set the associated effect
        // of an animation (this procedure) on previous animation passing null as new effect.
        if let Some(effect) = effect &&
            let Some(previous_animation) = effect.associated_animation() &&
            &*previous_animation != self
        {
            previous_animation.set_the_associated_effect(cx, None);
        }

        // Step 6. Let the associated effect of animation be new effect.
        if let Some(old_effect) = old_effect {
            old_effect.set_associated_animation(None);
        }
        if let Some(effect) = effect {
            effect.set_associated_animation(Some(self));
        }
        self.associated_effect.set(effect);

        // Step 7. Run the procedure to update an animation's finished state for animation with
        // the did seek flag set to false, and the synchronously notify flag set to false.
        self.update_finished_state(cx, false, false);
    }

    /// <https://drafts.csswg.org/web-animations-1/#silently-set-the-current-time>
    fn silently_set_the_current_time(&self, seek_time: Option<f64>) -> ErrorResult {
        // Step 1. If seek time is an unresolved time value, then perform the following steps.
        let Some(seek_time) = seek_time else {
            // Step 1.1 If the current time is resolved, then throw a TypeError.
            if self.current_time().is_some() {
                return Err(Error::Type(
                    c"Can't set the current time of a playing animation to null".to_owned(),
                ));
            }
            // Step 1.2 Abort these steps.
            return Ok(());
        };

        // Step 2. Update either animation's hold time or start time as follows:
        // If any of the following conditions are true:
        // * animation's hold time is resolved, or
        // * animation's start time is unresolved, or
        // * animation has no associated timeline or the associated timeline is inactive, or
        // * animation's playback rate is 0,
        // Set animation's hold time to seek time.
        let timeline_time = self.timeline_time();
        if self.hold_time.get().is_some() ||
            self.start_time.get().is_none() ||
            timeline_time.is_none() ||
            self.playback_rate.get() == 0.0
        {
            self.hold_time.set(Some(seek_time));
        } else if let Some(timeline_time) = timeline_time {
            // Otherwise, Set animation's start time to the result of evaluating
            // timeline time - (seek time / playback rate) where timeline time is the current
            // time value of timeline associated with animation.
            self.start_time
                .set(Some(timeline_time - seek_time / self.playback_rate.get()));
        }

        // Step 3. If animation has no associated timeline or the associated timeline is
        // inactive, make animation's start time unresolved.
        if timeline_time.is_none() {
            self.start_time.set(None);
        }

        // Step 4. Make animation's previous current time unresolved.
        self.previous_current_time.set(None);
        Ok(())
    }

    /// <https://drafts.csswg.org/web-animations-1/#set-the-current-time>
    fn set_the_current_time(&self, cx: &mut JSContext, seek_time: Option<f64>) -> ErrorResult {
        // Step 1. Run the steps to silently set the current time of animation to seek time.
        self.silently_set_the_current_time(seek_time)?;

        // Step 2. If animation has a pending pause task, synchronously complete the pause
        // operation by performing the following steps:
        if self.pending_pause_task.get() {
            // Step 2.1 Set animation's hold time to seek time.
            self.hold_time.set(seek_time);
            // Step 2.2 Apply any pending playback rate to animation.
            self.apply_any_pending_playback_rate();
            // Step 2.3 Make animation's start time unresolved.
            self.start_time.set(None);
            // Step 2.4 Cancel the pending pause task.
            self.pending_pause_task.set(false);
            // Step 2.5 Resolve animation's current ready promise with animation.
            self.resolve_ready_promise(cx);
        }

        // Step 3. Run the procedure to update an animation's finished state for animation with
        // the did seek flag set to true, and the synchronously notify flag set to false.
        self.update_finished_state(cx, true, false);
        Ok(())
    }

    /// <https://drafts.csswg.org/web-animations-1/#set-the-start-time>
    fn set_the_start_time(&self, cx: &mut JSContext, new_start_time: Option<f64>) {
        // Step 1. Let timeline time be the current time value of the timeline that animation
        // is associated with. If there is no timeline associated with animation or the
        // associated timeline is inactive, let the timeline time be unresolved.
        let timeline_time = self.timeline_time();

        // Step 2. If timeline time is unresolved and new start time is resolved, make
        // animation's hold time unresolved.
        if timeline_time.is_none() && new_start_time.is_some() {
            self.hold_time.set(None);
        }

        // Step 3. Let previous current time be animation's current time.
        let previous_current_time = self.current_time();

        // Step 4. Apply any pending playback rate on animation.
        self.apply_any_pending_playback_rate();

        // Step 5. Set animation's start time to new start time.
        self.start_time.set(new_start_time);

        // Step 6. Update animation's hold time based on the first matching condition from the
        // following,
        if new_start_time.is_some() {
            // If new start time is resolved, If animation's playback rate is not zero, make
            // animation's hold time unresolved.
            if self.playback_rate.get() != 0.0 {
                self.hold_time.set(None);
            }
        } else {
            // Otherwise (new start time is unresolved), Set animation's hold time to previous
            // current time even if previous current time is unresolved.
            self.hold_time.set(previous_current_time);
        }

        // Step 7. If animation has a pending play task or a pending pause task, cancel that
        // task and resolve animation's current ready promise with animation.
        if self.has_pending_task() {
            self.pending_play_task.set(false);
            self.pending_pause_task.set(false);
            self.resolve_ready_promise(cx);
        }

        // Step 8. Run the procedure to update an animation's finished state for animation with
        // the did seek flag set to true, and the synchronously notify flag set to false.
        self.update_finished_state(cx, true, false);
    }

    /// <https://drafts.csswg.org/web-animations-1/#play-an-animation>
    fn play_an_animation(&self, cx: &mut JSContext, auto_rewind: bool) -> ErrorResult {
        // Step 1. Let aborted pause be a boolean flag that is true if animation has a pending
        // pause task, and false otherwise.
        let aborted_pause = self.pending_pause_task.get();

        // Step 2. Let has pending ready promise be a boolean flag that is initially false.
        let mut has_pending_ready_promise = false;

        // Step 3. Let seek time be a time value that is initially unresolved.
        let mut seek_time = None;

        // Step 4. If the auto-rewind flag is true, perform the steps corresponding to the
        // first matching condition from the following, if any:
        let current_time = self.current_time();
        let effect_end = self.associated_effect_end();
        if auto_rewind {
            let rate = self.effective_playback_rate();
            if rate >= 0.0 && current_time.is_none_or(|time| time < 0.0 || time >= effect_end) {
                // If animation's effective playback rate ≥ 0, and animation's current time is
                // either: unresolved, or less than zero, or greater than or equal to associated
                // effect end, Set seek time to zero.
                seek_time = Some(0.0);
            } else if rate < 0.0 && current_time.is_none_or(|time| time <= 0.0 || time > effect_end)
            {
                // If animation's effective playback rate < 0, and animation's current time is
                // either: unresolved, or less than or equal to zero, or greater than associated
                // effect end,
                // If associated effect end is positive infinity, throw an "InvalidStateError"
                // DOMException and abort these steps.
                if effect_end.is_infinite() {
                    return Err(Error::InvalidState(None));
                }
                // Otherwise, Set seek time to animation's associated effect end.
                seek_time = Some(effect_end);
            }
        }

        // Step 5. If the following three conditions are all satisfied: seek time is unresolved,
        // and animation's start time is unresolved, and animation's current time is
        // unresolved, set seek time to zero.
        if seek_time.is_none() && self.start_time.get().is_none() && current_time.is_none() {
            seek_time = Some(0.0);
        }

        // Step 6. If seek time is resolved, set animation's hold time to seek time.
        if seek_time.is_some() {
            self.hold_time.set(seek_time);
        }

        // Step 7. If animation's hold time is resolved, let its start time be unresolved.
        if self.hold_time.get().is_some() {
            self.start_time.set(None);
        }

        // Step 8. If animation has a pending play task or a pending pause task, cancel that
        // task and set has pending ready promise to true.
        if self.has_pending_task() {
            self.pending_play_task.set(false);
            self.pending_pause_task.set(false);
            has_pending_ready_promise = true;
        }

        // Step 9. If the following four conditions are all satisfied: animation's hold time
        // is unresolved, and seek time is unresolved, and aborted pause is false, and
        // animation does not have a pending playback rate, abort this procedure.
        if self.hold_time.get().is_none() &&
            seek_time.is_none() &&
            !aborted_pause &&
            self.pending_playback_rate.get().is_none()
        {
            return Ok(());
        }

        // Step 10. If has pending ready promise is false, let animation's current ready
        // promise be a new promise in the relevant Realm of animation.
        if !has_pending_ready_promise {
            self.create_new_ready_promise(cx);
        }

        // Step 11. Schedule a task to run as soon as animation is ready.
        self.pending_play_task.set(true);

        // Step 12. Run the procedure to update an animation's finished state for animation
        // with the did seek flag set to false, and the synchronously notify flag set to false.
        self.update_finished_state(cx, false, false);
        Ok(())
    }

    /// The task scheduled by <https://drafts.csswg.org/web-animations-1/#play-an-animation>.
    fn run_pending_play_task(&self, cx: &mut JSContext) {
        self.pending_play_task.set(false);

        // Step 1. Assert that at least one of animation's start time or hold time is resolved.
        // Step 2. Let ready time be the time value of the timeline associated with animation
        // at the moment when animation became ready.
        let Some(ready_time) = self.timeline_time() else {
            // Without an active timeline the animation can't start: it stays paused.
            self.resolve_ready_promise(cx);
            return;
        };

        // Step 3. Perform the steps corresponding to the first matching condition below, if
        // any:
        if let Some(hold_time) = self.hold_time.get() {
            // If animation's hold time is resolved,
            // Step 3.1 Apply any pending playback rate on animation.
            self.apply_any_pending_playback_rate();
            // Step 3.2 Let new start time be the result of evaluating
            // ready time - hold time / playback rate for animation. If the playback rate is
            // zero, let new start time be simply ready time.
            let rate = self.playback_rate.get();
            let new_start_time = if rate == 0.0 {
                ready_time
            } else {
                ready_time - hold_time / rate
            };
            // Step 3.3 Set the start time of animation to new start time.
            self.start_time.set(Some(new_start_time));
            // Step 3.4 If animation's playback rate is not 0, make animation's hold time
            // unresolved.
            if rate != 0.0 {
                self.hold_time.set(None);
            }
        } else if let Some(start_time) = self.start_time.get() &&
            self.pending_playback_rate.get().is_some()
        {
            // If animation's start time is resolved and animation has a pending playback rate,
            // Step 3.1 Let current time to match be the result of evaluating
            // (ready time - start time) × playback rate for animation.
            let current_time_to_match = (ready_time - start_time) * self.playback_rate.get();
            // Step 3.2 Apply any pending playback rate on animation.
            self.apply_any_pending_playback_rate();
            let rate = self.playback_rate.get();
            // Step 3.3 If animation's playback rate is zero, let animation's hold time be
            // current time to match.
            if rate == 0.0 {
                self.hold_time.set(Some(current_time_to_match));
            }
            // Step 3.4 Let new start time be the result of evaluating
            // ready time - current time to match / playback rate for animation. If the
            // playback rate is zero, let new start time be simply ready time.
            let new_start_time = if rate == 0.0 {
                ready_time
            } else {
                ready_time - current_time_to_match / rate
            };
            // Step 3.5 Set the start time of animation to new start time.
            self.start_time.set(Some(new_start_time));
        }

        // Step 4. Resolve animation's current ready promise with animation.
        self.resolve_ready_promise(cx);

        // Step 5. Run the procedure to update an animation's finished state for animation with
        // the did seek flag set to false, and the synchronously notify flag set to false.
        self.update_finished_state(cx, false, false);
    }

    /// <https://drafts.csswg.org/web-animations-1/#pause-an-animation>
    fn pause_an_animation(&self, cx: &mut JSContext) -> ErrorResult {
        // Step 1. If animation has a pending pause task, abort these steps.
        // Step 2. If the play state of animation is paused, abort these steps.
        if self.pending_pause_task.get() || self.play_state() == AnimationPlayState::Paused {
            return Ok(());
        }

        // Step 3. Let seek time be a time value that is initially unresolved.
        let mut seek_time = None;

        // Step 4. Let has finite timeline be true if animation has an associated timeline that
        // is not monotonically increasing. (Document timelines are monotonically increasing.)

        // Step 5. If the animation's current time is unresolved, perform the steps according
        // to the first matching condition below:
        if self.current_time().is_none() {
            if self.playback_rate.get() >= 0.0 {
                // If animation's playback rate is ≥ 0, Set seek time to zero.
                seek_time = Some(0.0);
            } else {
                // Otherwise, If associated effect end for animation is positive infinity,
                // throw an "InvalidStateError" DOMException and abort these steps.
                let effect_end = self.associated_effect_end();
                if effect_end.is_infinite() {
                    return Err(Error::InvalidState(None));
                }
                // Otherwise, Set seek time to animation's associated effect end.
                seek_time = Some(effect_end);
            }
        }

        // Step 6. If seek time is resolved, set animation's hold time to seek time.
        if seek_time.is_some() {
            self.hold_time.set(seek_time);
        }

        // Step 7. Let has pending ready promise be a boolean flag that is initially false.
        // Step 8. If animation has a pending play task, cancel that task and let has pending
        // ready promise be true.
        let has_pending_ready_promise = self.pending_play_task.replace(false);

        // Step 9. If has pending ready promise is false, set animation's current ready promise
        // to a new promise in the relevant Realm of animation.
        if !has_pending_ready_promise {
            self.create_new_ready_promise(cx);
        }

        // Step 10. Schedule a task to be executed at the first possible moment where all of
        // the following conditions are true: the user agent has performed any processing
        // necessary to suspend the playback of animation's associated effect, if any; the
        // animation is associated with a timeline that is not inactive.
        self.pending_pause_task.set(true);

        // Step 11. Run the procedure to update an animation's finished state for animation
        // with the did seek flag set to false, and the synchronously notify flag set to false.
        self.update_finished_state(cx, false, false);
        Ok(())
    }

    /// The task scheduled by <https://drafts.csswg.org/web-animations-1/#pause-an-animation>.
    fn run_pending_pause_task(&self, cx: &mut JSContext) {
        self.pending_pause_task.set(false);

        // Step 1. Let ready time be the time value of the timeline associated with animation
        // at the moment when the user agent completed processing necessary to suspend
        // playback of animation's associated effect.
        let ready_time = self.timeline_time();

        // Step 2. If animation's start time is resolved and its hold time is not resolved,
        // let animation's hold time be the result of evaluating
        // (ready time - start time) × playback rate.
        if let (Some(start_time), None, Some(ready_time)) =
            (self.start_time.get(), self.hold_time.get(), ready_time)
        {
            self.hold_time
                .set(Some((ready_time - start_time) * self.playback_rate.get()));
        }

        // Step 3. Apply any pending playback rate on animation.
        self.apply_any_pending_playback_rate();

        // Step 4. Make animation's start time unresolved.
        self.start_time.set(None);

        // Step 5. Resolve animation's current ready promise with animation.
        self.resolve_ready_promise(cx);

        // Step 6. Run the procedure to update an animation's finished state for animation with
        // the did seek flag set to false, and the synchronously notify flag set to false.
        self.update_finished_state(cx, false, false);
    }

    /// <https://drafts.csswg.org/web-animations-1/#update-an-animations-finished-state>
    fn update_finished_state(
        &self,
        cx: &mut JSContext,
        did_seek: bool,
        synchronously_notify: bool,
    ) {
        // Step 1. Let the unconstrained current time be the result of calculating the current
        // time substituting an unresolved time value for the hold time if did seek is false.
        // If did seek is true, the unconstrained current time is equal to the current time.
        let unconstrained_current_time = self.calculate_current_time(!did_seek);

        // Step 2. If all three of the following conditions are true,
        // * the unconstrained current time is resolved, and
        // * animation's start time is resolved, and
        // * animation does not have a pending play task or a pending pause task,
        // then update animation's hold time based on the first matching condition for
        // animation from below, if any:
        if let (Some(unconstrained_current_time), Some(_)) =
            (unconstrained_current_time, self.start_time.get()) &&
            !self.has_pending_task()
        {
            let rate = self.playback_rate.get();
            let effect_end = self.associated_effect_end();
            let previous_current_time = self.previous_current_time.get();
            if rate > 0.0 && unconstrained_current_time >= effect_end {
                // If playback rate > 0 and unconstrained current time is greater than or equal
                // to associated effect end,
                self.hold_time.set(Some(if did_seek {
                    unconstrained_current_time
                } else {
                    previous_current_time.map_or(effect_end, |time| time.max(effect_end))
                }));
            } else if rate < 0.0 && unconstrained_current_time <= 0.0 {
                // If playback rate < 0 and unconstrained current time is less than or equal to
                // 0,
                self.hold_time.set(Some(if did_seek {
                    unconstrained_current_time
                } else {
                    previous_current_time.map_or(0.0, |time| time.min(0.0))
                }));
            } else if rate != 0.0 &&
                let Some(timeline_time) = self.timeline_time()
            {
                // If playback rate ≠ 0, and animation is associated with an active timeline,
                // Perform the following steps:
                // Step 2.1 If did seek is true and the hold time is resolved, let animation's
                // start time be equal to the result of evaluating
                // timeline time - (hold time / playback rate).
                if did_seek && let Some(hold_time) = self.hold_time.get() {
                    self.start_time.set(Some(timeline_time - hold_time / rate));
                }
                // Step 2.2 Let the hold time be unresolved.
                self.hold_time.set(None);
            }
        }

        // Step 3. Set the previous current time of animation be the result of calculating its
        // current time.
        self.previous_current_time.set(self.current_time());

        // Step 4. Let current finished state be true if the play state of animation is
        // finished. Otherwise, let it be false.
        let current_finished_state = self.play_state() == AnimationPlayState::Finished;

        // Step 5. If current finished state is true and the current finished promise is not
        // yet resolved, perform the following steps:
        let finished_promise_settled = self.finished_promise.borrow().is_fulfilled();
        if current_finished_state && !finished_promise_settled {
            if synchronously_notify {
                // If synchronously notify is true, cancel any queued microtask to run the
                // finish notification steps for this animation, and run the finish
                // notification steps immediately.
                self.finish_notification_queued.set(false);
                self.finish_notification_steps(cx);
            } else if !self.finish_notification_queued.get() {
                // Otherwise, if synchronously notify is false, queue a microtask to run finish
                // notification steps for animation unless there is already a microtask queued
                // to run those steps for animation.
                self.finish_notification_queued.set(true);
                self.global().enqueue_microtask(
                    cx,
                    Box::new(FinishNotificationMicrotask {
                        animation: Dom::from_ref(self),
                    }),
                );
            }
        }

        // Step 6. If current finished state is false and animation's current finished promise
        // is already resolved, set animation's current finished promise to a new promise in
        // the relevant Realm of animation.
        if !current_finished_state && finished_promise_settled {
            let promise = Promise::new(cx, &self.global());
            *self.finished_promise.borrow_mut() = promise.to_traced();
        }

        self.update_registration();
        self.update_effect_values(cx);
        self.schedule_wake_up();
    }

    /// Update the values this animation's effect applies to its target (see
    /// [`KeyframeEffect::effect_keyframe_index`]), restyling the target if they changed.
    pub(crate) fn update_effect_values(&self, cx: &mut JSContext) {
        let Some(effect) = self
            .associated_effect
            .get()
            .and_then(DomRoot::downcast::<KeyframeEffect>)
        else {
            return;
        };
        let index = if self.play_state() == AnimationPlayState::Idle {
            None
        } else {
            effect.effect_keyframe_index()
        };
        if effect.set_applied_keyframe(index) &&
            let Some(target) = effect.target()
        {
            apply_script_animations_to(cx, &target);
        }
    }

    /// The finish notification steps of
    /// <https://drafts.csswg.org/web-animations-1/#update-an-animations-finished-state>.
    fn finish_notification_steps(&self, cx: &mut JSContext) {
        // Step 1. If animation's play state is not equal to finished, abort these steps.
        if self.play_state() != AnimationPlayState::Finished {
            return;
        }

        // Step 2. Resolve animation's current finished promise object with animation.
        let promise = self.finished_promise.borrow().root(cx);
        if !promise.is_fulfilled() {
            promise.resolve_native(cx, self);
        }

        // Step 3. Create an AnimationPlaybackEvent, finishEvent.
        // Step 4. Set finishEvent's type attribute to finish.
        // Step 5. Set finishEvent's currentTime attribute to the current time of animation.
        // Step 6. Set finishEvent's timelineTime attribute to the current time of the timeline
        // with which animation is associated.
        // Step 7. If animation has a document for timing, then append finishEvent to its
        // document for timing's pending animation event queue along with its target,
        // animation. Otherwise, queue a task to dispatch finishEvent at animation.
        self.queue_playback_event(
            Atom::from("finish"),
            self.current_time(),
            self.timeline_time(),
        );
    }

    fn queue_playback_event(
        &self,
        event_type: Atom,
        current_time: Option<f64>,
        timeline_time: Option<f64>,
    ) {
        let trusted_animation = Trusted::new(self);
        self.global()
            .task_manager()
            .dom_manipulation_task_source()
            .queue(task!(fire_animation_playback_event: move |cx| {
                let animation = trusted_animation.root();
                let event = AnimationPlaybackEvent::new(
                    cx,
                    &animation.window(),
                    event_type,
                    current_time,
                    timeline_time,
                );
                event.upcast::<Event>().fire(cx, animation.upcast());
            }));
    }

    /// Schedule a timer that runs pending tasks and notices when the animation finishes.
    fn schedule_wake_up(&self) {
        let delay = if self.has_pending_task() {
            0.0
        } else {
            if self.play_state() != AnimationPlayState::Running {
                return;
            }
            let rate = self.playback_rate.get();
            let Some(current_time) = self.current_time() else {
                return;
            };
            let target_time = if rate > 0.0 {
                self.associated_effect_end()
            } else if rate < 0.0 {
                0.0
            } else {
                return;
            };
            let mut delay = (target_time - current_time) / rate;

            // Wake up when the effect starts after its delay, and regularly while it is
            // active if the keyframe it applies can change.
            if let Some(effect) = self
                .associated_effect
                .get()
                .and_then(DomRoot::downcast::<KeyframeEffect>)
            {
                let state = effect.upcast::<AnimationEffect>().calculate_timing();
                if state.phase == EffectPhase::Before && rate > 0.0 {
                    if let Some(local_time) = state.local_time {
                        delay = delay.min((state.start_delay - local_time) / rate);
                    }
                } else if state.phase == EffectPhase::Active && effect.needs_periodic_updates() {
                    delay = delay.min(100.0);
                }
            }
            if !delay.is_finite() {
                return;
            }
            delay.max(1.0)
        };

        let generation = self.timer_generation.get().wrapping_add(1);
        self.timer_generation.set(generation);
        let trusted_animation = Trusted::new(self);
        let duration = Duration::from_millis(delay.ceil().min(u32::MAX as f64) as u64);
        with_script_thread(|script_thread| {
            if let Some(timer_id) = self.timer_id.take() {
                script_thread.cancel_timer(timer_id);
            }
            let timer_id = script_thread.schedule_timer(TimerEventRequest {
                callback: Box::new(move || {
                    let animation = trusted_animation.root();
                    animation.timer_id.set(None);
                    if animation.timer_generation.get() != generation {
                        return;
                    }
                    // Timer callbacks don't get a JSContext; continue in a task.
                    let trusted_animation = Trusted::new(&*animation);
                    animation
                        .global()
                        .task_manager()
                        .dom_manipulation_task_source()
                        .queue(task!(web_animation_wake_up: move |cx| {
                            let animation = trusted_animation.root();
                            if animation.timer_generation.get() == generation {
                                animation.wake_up(cx);
                            }
                        }));
                }),
                duration,
            });
            self.timer_id.set(Some(timer_id));
        });
    }

    fn wake_up(&self, cx: &mut JSContext) {
        self.refresh_timeline();
        if self.pending_play_task.get() {
            self.run_pending_play_task(cx);
        } else if self.pending_pause_task.get() {
            self.run_pending_pause_task(cx);
        } else {
            self.update_finished_state(cx, false, false);
        }
    }

    /// <https://drafts.csswg.org/web-animations-1/#relevant-animations>
    pub(crate) fn is_relevant(&self) -> bool {
        self.associated_effect.get().is_some_and(|effect| {
            let state = effect.calculate_timing();
            state.is_current() || state.is_in_effect()
        })
    }

    /// The target element of this animation's keyframe effect, if any.
    pub(crate) fn target_element(&self) -> Option<DomRoot<crate::dom::element::Element>> {
        self.associated_effect
            .get()
            .and_then(|effect| DomRoot::downcast::<KeyframeEffect>(effect))
            .and_then(|effect| effect.target())
    }

    /// Keep the document's list of animations (used by `getAnimations()`) up to date.
    fn update_registration(&self) {
        let document = self.window().Document();
        let timeline = document.Timeline();
        if self.play_state() != AnimationPlayState::Idle && self.is_relevant() {
            timeline.register_animation(self);
        } else {
            timeline.unregister_animation(self);
        }
    }
}

impl AnimationMethods<crate::DomTypeHolder> for Animation {
    /// <https://drafts.csswg.org/web-animations-1/#dom-animation-animation>
    fn Constructor(
        cx: &mut JSContext,
        window: &Window,
        proto: Option<HandleObject>,
        effect: Option<&AnimationEffect>,
        timeline: Option<Option<&AnimationTimeline>>,
    ) -> DomRoot<Self> {
        // Step 1. Let animation be a new Animation object.
        let animation = Animation::new_with_proto(cx, window.upcast(), proto);

        // Step 2. Run the procedure to set the timeline of an animation on animation passing timeline
        // as the new timeline; or, if the timeline argument is missing, passing the default document
        // timeline of the Document associated with the Window that is the current global object.
        let timeline = match timeline {
            Some(timeline) => timeline.map(DomRoot::from_ref),
            None => Some(DomRoot::upcast(window.Document().Timeline())),
        };
        animation.set_the_timeline(cx, timeline.as_deref());

        // Step 3. Run the procedure to set the associated effect of an animation on animation passing
        // source as the new effect.
        animation.set_the_associated_effect(cx, effect);

        animation
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-animation-id>
    fn Id(&self) -> DOMString {
        self.id.borrow().clone()
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-animation-id>
    fn SetId(&self, id: DOMString) {
        self.set_id(id);
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-animation-effect>
    fn GetEffect(&self) -> Option<DomRoot<AnimationEffect>> {
        self.associated_effect.get()
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-animation-effect>
    fn SetEffect(&self, cx: &mut JSContext, effect: Option<&AnimationEffect>) {
        // > Setting this attribute updates the object's associated effect using
        // > the procedure to set the associated effect of an animation.
        self.refresh_timeline();
        self.set_the_associated_effect(cx, effect);
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-animation-timeline>
    fn GetTimeline(&self) -> Option<DomRoot<AnimationTimeline>> {
        self.timeline.get()
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-animation-timeline>
    fn SetTimeline(&self, cx: &mut JSContext, timeline: Option<&AnimationTimeline>) {
        // > Setting this attribute updates the object's timeline using the procedure
        // > to set the timeline of an animation.
        self.refresh_timeline();
        self.set_the_timeline(cx, timeline);
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-animation-starttime>
    fn GetStartTime(&self) -> Option<Finite<f64>> {
        self.start_time.get().and_then(Finite::new)
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-animation-starttime>
    fn SetStartTime(&self, cx: &mut JSContext, start_time: Option<Finite<f64>>) {
        self.refresh_timeline();
        self.set_the_start_time(cx, start_time.map(|time| *time));
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-animation-currenttime>
    fn GetCurrentTime(&self) -> Option<Finite<f64>> {
        self.refresh_timeline();
        self.current_time().and_then(Finite::new)
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-animation-currenttime>
    fn SetCurrentTime(&self, cx: &mut JSContext, current_time: Option<Finite<f64>>) -> ErrorResult {
        self.refresh_timeline();
        self.set_the_current_time(cx, current_time.map(|time| *time))
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-animation-playbackrate>
    fn PlaybackRate(&self) -> Finite<f64> {
        Finite::wrap(self.playback_rate.get())
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-animation-playbackrate>
    fn SetPlaybackRate(&self, cx: &mut JSContext, playback_rate: Finite<f64>) {
        // <https://drafts.csswg.org/web-animations-1/#set-the-playback-rate>
        self.refresh_timeline();
        // Step 1. Clear any pending playback rate on animation.
        self.pending_playback_rate.set(None);
        // Step 2. Let previous time be the value of the current time of animation before
        // changing the playback rate.
        let previous_time = self.current_time();
        // Step 3. Let previous playback rate be the current effective playback rate of
        // animation.
        // Step 4. Set the playback rate to new playback rate.
        self.playback_rate.set(*playback_rate);
        // Step 5. Perform the steps corresponding to the first matching condition from the
        // following, if any:
        // (Monotonically increasing timelines:) If previous time is resolved, set the current
        // time of animation to previous time.
        if previous_time.is_some() {
            let _ = self.set_the_current_time(cx, previous_time);
        } else {
            self.update_finished_state(cx, false, false);
        }
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-animation-playstate>
    fn PlayState(&self) -> AnimationPlayState {
        self.refresh_timeline();
        self.play_state()
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-animation-replacestate>
    fn ReplaceState(&self) -> AnimationReplaceState {
        // Stillwater never removes replaced animations, so they are only active or persisted.
        if self.persisted.get() {
            AnimationReplaceState::Persisted
        } else {
            AnimationReplaceState::Active
        }
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-animation-pending>
    fn Pending(&self) -> bool {
        self.has_pending_task()
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-animation-ready>
    fn Ready(&self, cx: &mut JSContext) -> RootedPromise {
        self.ready_promise.borrow().root(cx)
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-animation-finished>
    fn Finished(&self, cx: &mut JSContext) -> RootedPromise {
        self.finished_promise.borrow().root(cx)
    }

    // <https://drafts.csswg.org/web-animations-1/#dom-animation-onfinish>
    event_handler!(finish, GetOnfinish, SetOnfinish);

    // <https://drafts.csswg.org/web-animations-1/#dom-animation-oncancel>
    event_handler!(cancel, GetOncancel, SetOncancel);

    // <https://drafts.csswg.org/web-animations-1/#dom-animation-onremove>
    event_handler!(remove, GetOnremove, SetOnremove);

    /// <https://drafts.csswg.org/web-animations-1/#dom-animation-cancel>
    fn Cancel(&self, cx: &mut JSContext) {
        // <https://drafts.csswg.org/web-animations-1/#cancel-an-animation>
        self.refresh_timeline();

        // Step 1. If animation's play state is not idle, perform the following steps:
        if self.play_state() != AnimationPlayState::Idle {
            // Step 1.1 Run the procedure to reset an animation's pending tasks on animation.
            // <https://drafts.csswg.org/web-animations-1/#reset-an-animations-pending-tasks>
            if self.has_pending_task() {
                self.pending_play_task.set(false);
                self.pending_pause_task.set(false);
                self.apply_any_pending_playback_rate();
                let ready_promise = self.ready_promise.borrow().root(cx);
                ready_promise.reject_error(cx, Error::Abort(None));
                ready_promise.set_promise_is_handled(cx);
                let promise = Promise::new(cx, &self.global());
                promise.resolve_native(cx, self);
                *self.ready_promise.borrow_mut() = promise.to_traced();
            }

            // Step 1.2 Reject the current finished promise with a DOMException named
            // "AbortError".
            // Step 1.3 Set the [[PromiseIsHandled]] internal slot of the current finished
            // promise to true.
            let finished_promise = self.finished_promise.borrow().root(cx);
            if !finished_promise.is_fulfilled() {
                finished_promise.reject_error(cx, Error::Abort(None));
                finished_promise.set_promise_is_handled(cx);
            }

            // Step 1.4 Let current finished promise be a new promise in the relevant Realm of
            // animation.
            let promise = Promise::new(cx, &self.global());
            *self.finished_promise.borrow_mut() = promise.to_traced();

            // Step 1.5 Create an AnimationPlaybackEvent, cancelEvent, whose type is cancel,
            // currentTime is null and timelineTime is the current time of the timeline with
            // which animation is associated, and dispatch it (via a queued task).
            self.queue_playback_event(Atom::from("cancel"), None, self.timeline_time());
        }

        // Step 2. Make animation's hold time unresolved.
        self.hold_time.set(None);

        // Step 3. Make animation's start time unresolved.
        self.start_time.set(None);

        self.timer_generation
            .set(self.timer_generation.get().wrapping_add(1));
        self.update_registration();
        self.update_effect_values(cx);
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-animation-finish>
    fn Finish(&self, cx: &mut JSContext) -> ErrorResult {
        // <https://drafts.csswg.org/web-animations-1/#finish-an-animation>
        self.refresh_timeline();

        // Step 1. If animation's effective playback rate is zero, or if animation's effective
        // playback rate > 0 and associated effect end is infinity, throw an
        // "InvalidStateError" DOMException and abort these steps.
        let rate = self.effective_playback_rate();
        let effect_end = self.associated_effect_end();
        if rate == 0.0 || (rate > 0.0 && effect_end.is_infinite()) {
            return Err(Error::InvalidState(None));
        }

        // Step 2. Apply any pending playback rate to animation.
        self.apply_any_pending_playback_rate();

        // Step 3. Set limit as follows: If playback rate > 0, let limit be associated effect
        // end. Otherwise, let limit be zero.
        let rate = self.playback_rate.get();
        let limit = if rate > 0.0 { effect_end } else { 0.0 };

        // Step 4. Silently set the current time to limit.
        self.silently_set_the_current_time(Some(limit))?;

        // Step 5. If animation's start time is unresolved and animation has an associated
        // active timeline, let the start time be the result of evaluating
        // timeline time - (limit / playback rate) where timeline time is the current time
        // value of the associated timeline.
        if self.start_time.get().is_none() &&
            let Some(timeline_time) = self.timeline_time()
        {
            self.start_time.set(Some(timeline_time - limit / rate));
        }

        // Step 6. If there is a pending pause task and start time is resolved,
        if self.pending_pause_task.get() && self.start_time.get().is_some() {
            // Step 6.1 Let the hold time be unresolved.
            self.hold_time.set(None);
            // Step 6.2 Cancel the pending pause task.
            self.pending_pause_task.set(false);
            // Step 6.3 Resolve the current ready promise of animation with animation.
            self.resolve_ready_promise(cx);
        }

        // Step 7. If there is a pending play task and start time is resolved, cancel that task
        // and resolve the current ready promise of animation with animation.
        if self.pending_play_task.get() && self.start_time.get().is_some() {
            self.pending_play_task.set(false);
            self.resolve_ready_promise(cx);
        }

        // Step 8. Run the procedure to update an animation's finished state for animation with
        // the did seek flag set to true, and the synchronously notify flag set to true.
        self.update_finished_state(cx, true, true);
        Ok(())
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-animation-play>
    fn Play(&self, cx: &mut JSContext) -> ErrorResult {
        self.refresh_timeline();
        self.play_an_animation(cx, true)
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-animation-pause>
    fn Pause(&self, cx: &mut JSContext) -> ErrorResult {
        self.refresh_timeline();
        self.pause_an_animation(cx)
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-animation-updateplaybackrate>
    fn UpdatePlaybackRate(&self, cx: &mut JSContext, playback_rate: Finite<f64>) {
        // <https://drafts.csswg.org/web-animations-1/#seamlessly-update-the-playback-rate>
        self.refresh_timeline();

        // Step 1. Let previous play state be animation's play state.
        let previous_play_state = self.play_state();

        // Step 2. Let animation's pending playback rate be new playback rate.
        self.pending_playback_rate.set(Some(*playback_rate));

        // Step 3. Perform the steps corresponding to the first matching condition below:
        if self.has_pending_task() {
            // If animation has a pending play task or a pending pause task, Abort these steps.
            return;
        }
        match previous_play_state {
            AnimationPlayState::Idle | AnimationPlayState::Paused => {
                // If previous play state is idle or paused, Apply any pending playback rate on
                // animation.
                self.apply_any_pending_playback_rate();
                self.update_finished_state(cx, false, false);
            },
            AnimationPlayState::Finished => {
                // If previous play state is finished,
                // Step 3.1 Let the unconstrained current time be the result of calculating the
                // current time of animation substituting an unresolved time value for the hold
                // time.
                let unconstrained_current_time = self.calculate_current_time(true);
                // Step 3.2 Let animation's start time be the result of evaluating the following
                // expression: timeline time - (unconstrained current time / pending playback
                // rate), where timeline time is the current time value of the timeline
                // associated with animation. If pending playback rate is zero, let animation's
                // start time be timeline time.
                if let (Some(timeline_time), Some(unconstrained_current_time)) =
                    (self.timeline_time(), unconstrained_current_time)
                {
                    let new_start_time = if *playback_rate == 0.0 {
                        timeline_time
                    } else {
                        timeline_time - unconstrained_current_time / *playback_rate
                    };
                    self.start_time.set(Some(new_start_time));
                }
                // Step 3.3 Apply any pending playback rate on animation.
                self.apply_any_pending_playback_rate();
                // Step 3.4 Run the procedure to update an animation's finished state for
                // animation with the did seek flag set to false, and the synchronously notify
                // flag set to false.
                self.update_finished_state(cx, false, false);
            },
            AnimationPlayState::Running => {
                // Otherwise, Run the procedure to play an animation for animation with the
                // auto-rewind flag set to false.
                let _ = self.play_an_animation(cx, false);
            },
        }
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-animation-reverse>
    fn Reverse(&self, cx: &mut JSContext) -> ErrorResult {
        // <https://drafts.csswg.org/web-animations-1/#reverse-an-animation>
        self.refresh_timeline();

        // Step 1. If there is no timeline associated with animation, or the associated
        // timeline is inactive throw an "InvalidStateError" DOMException and abort these steps.
        if self.timeline_time().is_none() {
            return Err(Error::InvalidState(None));
        }

        // Step 2. Let original pending playback rate be animation's pending playback rate.
        let original_pending_playback_rate = self.pending_playback_rate.get();

        // Step 3. Let animation's pending playback rate be the additive inverse of its
        // effective playback rate (i.e. -effective playback rate).
        self.pending_playback_rate
            .set(Some(-self.effective_playback_rate()));

        // Step 4. Run the steps to play an animation for animation with the auto-rewind flag
        // set to true.
        // If the steps to play an animation throw an exception, set animation's pending
        // playback rate to original pending playback rate and propagate the exception.
        let result = self.play_an_animation(cx, true);
        if result.is_err() {
            self.pending_playback_rate
                .set(original_pending_playback_rate);
        }
        result
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-animation-persist>
    fn Persist(&self) {
        // > Sets this animation's replace state to persisted.
        self.persisted.set(true);
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-animation-commitstyles>
    fn CommitStyles(&self, cx: &mut JSContext) -> ErrorResult {
        self.refresh_timeline();
        let Some(effect) = self
            .associated_effect
            .get()
            .and_then(DomRoot::downcast::<KeyframeEffect>)
        else {
            return Ok(());
        };
        effect.commit_styles(cx)
    }
}

/// Runs the finish notification steps of an animation in a microtask.
#[derive(JSTraceable, MallocSizeOf)]
#[cfg_attr(crown, crown::unrooted_must_root_lint::must_root)]
struct FinishNotificationMicrotask {
    animation: Dom<Animation>,
}

impl MicrotaskRunnable for FinishNotificationMicrotask {
    fn handler(&self, cx: &mut JSContext) {
        let mut realm = enter_auto_realm(cx, &*self.animation);
        let cx = &mut realm;
        if !self.animation.finish_notification_queued.replace(false) {
            return;
        }
        self.animation.finish_notification_steps(cx);
    }
}
