/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use js::context::JSContext;
use js::gc::HandleObject;
use num_traits::ToPrimitive;
use script_bindings::cell::DomRefCell;
use script_bindings::codegen::GenericBindings::DocumentTimelineBinding::DocumentTimelineOptions;
use script_bindings::reflector::{reflect_dom_object, reflect_dom_object_with_proto};
use script_bindings::root::DomRoot;
use servo_base::cross_process_instant::CrossProcessInstant;
use servo_config::pref;
use time::Duration;

use crate::dom::bindings::codegen::Bindings::DocumentTimelineBinding::DocumentTimelineMethods;
use crate::dom::bindings::root::Dom;
use crate::dom::types::{Animation, AnimationTimeline, Window};

/// <https://drafts.csswg.org/web-animations-1/#the-documenttimeline-interface>
#[dom_struct]
pub(crate) struct DocumentTimeline {
    animation_timeline: AnimationTimeline,
    /// An offset from the `Document`'s time origin as a [`Duration`] offset. This is determined by the original
    /// "originTime" specified during construction of the [`AnimationTimeline`] in the options object.
    /// Note that this value might be negative.
    ///
    /// See:
    ///   - <https://drafts.csswg.org/web-animations-1/#dom-documenttimelineoptions-origintime>
    ///   - <https://html.spec.whatwg.org/multipage/#concept-settings-object-time-origin>
    #[no_trace]
    origin_offset: Duration,
    /// The script-created animations of this document that are still relevant, for
    /// Document.getAnimations() and Element.getAnimations(). Only the document's default
    /// timeline keeps this list.
    animations: DomRefCell<Vec<Dom<Animation>>>,
}

impl DocumentTimeline {
    fn new_with_duration(
        cx: &mut JSContext,
        window: &Window,
        proto: Option<HandleObject>,
        origin_time: Duration,
    ) -> DomRoot<Self> {
        let duration_since_time_origin =
            CrossProcessInstant::now() - window.navigation_start() - origin_time;
        reflect_dom_object_with_proto(
            cx,
            Box::new(Self {
                animation_timeline: AnimationTimeline::new_inherited(duration_since_time_origin),
                origin_offset: origin_time,
                animations: Default::default(),
            }),
            window,
            proto,
        )
    }

    pub(crate) fn new(cx: &mut JSContext, window: &Window) -> DomRoot<DocumentTimeline> {
        let duration = if pref!(layout_animations_test_enabled) {
            Duration::ZERO
        } else {
            CrossProcessInstant::now() - window.navigation_start()
        };
        reflect_dom_object(
            cx,
            Box::new(Self {
                animation_timeline: AnimationTimeline::new_inherited(duration),
                origin_offset: Duration::ZERO,
                animations: Default::default(),
            }),
            window,
        )
    }

    /// Updates the value of the `AnimationTimeline` to the current clock time.
    pub(crate) fn update(&self, window: &Window) {
        let duration_since_time_origin =
            CrossProcessInstant::now() - window.navigation_start() - self.origin_offset;
        self.animation_timeline
            .set_current_time(duration_since_time_origin);
    }

    pub(crate) fn register_animation(&self, animation: &Animation) {
        let mut animations = self.animations.borrow_mut();
        if !animations.iter().any(|existing| &**existing == animation) {
            animations.push(Dom::from_ref(animation));
        }
    }

    pub(crate) fn unregister_animation(&self, animation: &Animation) {
        self.animations
            .borrow_mut()
            .retain(|existing| &**existing != animation);
    }

    /// The registered animations, in the order they were registered (an approximation of
    /// <https://drafts.csswg.org/web-animations-1/#animation-composite-order>).
    pub(crate) fn animations(&self) -> Vec<DomRoot<Animation>> {
        self.animations
            .borrow()
            .iter()
            .map(|animation| animation.as_rooted())
            .collect()
    }

    /// Increments the current value of the timeline by a specific number of seconds.
    /// This is used for testing.
    pub(crate) fn advance_specific(&self, by: Duration) {
        self.animation_timeline.advance_specific(by);
    }
}

impl DocumentTimelineMethods<crate::DomTypeHolder> for DocumentTimeline {
    fn Constructor(
        cx: &mut JSContext,
        window: &Window,
        proto: Option<HandleObject>,
        options: &DocumentTimelineOptions,
    ) -> DomRoot<Self> {
        Self::new_with_duration(
            cx,
            window,
            proto,
            Duration::seconds_f64(options.originTime.to_f64().unwrap_or_default() / 1000.),
        )
    }
}
