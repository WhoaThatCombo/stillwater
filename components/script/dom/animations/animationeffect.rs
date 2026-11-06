/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use cssparser::Parser;
use dom_struct::dom_struct;
use script_bindings::cell::DomRefCell;
use script_bindings::codegen::GenericBindings::AnimationEffectBinding::{
    AnimationEffectMethods, ComputedEffectTiming, EffectTiming, FillMode, OptionalEffectTiming,
    PlaybackDirection,
};
use script_bindings::codegen::GenericBindings::WindowBinding::WindowMethods;
use script_bindings::error::{Error, Fallible};
use script_bindings::num::Finite;
use script_bindings::reflector::Reflector;
use script_bindings::root::{Dom, DomRoot};
use script_bindings::str::DOMString;
use style::parser::Parse;
use style::stylesheets::CssRuleType;
use style::values::generics::easing::{BeforeFlag, TimingKeyword};
use style::values::specified::TimingFunction;
use style_traits::{ParsingMode, ToCss};

use crate::css::css::parser_context_for_document;
use crate::dom::Window;
use crate::dom::bindings::codegen::UnionTypes::UnrestrictedDoubleOrString;
use crate::dom::bindings::root::MutNullableDom;
use crate::dom::types::Animation;

/// <https://drafts.csswg.org/web-animations-1/#animationeffect>
#[dom_struct]
pub(crate) struct AnimationEffect {
    reflector: Reflector,

    /// The window that this `AnimationEffect` was constructed in.
    window: Dom<Window>,

    specified_timing_properties: DomRefCell<SpecifiedTimingProperties>,

    /// <https://drafts.csswg.org/web-animations-1/#animation-associated-effect>, seen from the effect.
    animation: MutNullableDom<Animation>,
}

#[derive(Clone, JSTraceable, MallocSizeOf)]
struct SpecifiedTimingProperties {
    /// <https://drafts.csswg.org/web-animations-1/#start-delay>
    start_delay: Finite<f64>,

    /// <https://drafts.csswg.org/web-animations-1/#end-delay>
    end_delay: Finite<f64>,

    /// <https://drafts.csswg.org/web-animations-1/#fill-mode>
    fill_mode: FillMode,

    /// <https://drafts.csswg.org/web-animations-1/#iteration-count>
    iteration_count: f64,

    /// <https://drafts.csswg.org/web-animations-1/#iteration-start>
    iteration_start: Finite<f64>,

    /// <https://drafts.csswg.org/web-animations-1/#iteration-duration>
    iteration_duration: IterationDurationOrAuto,

    /// <https://drafts.csswg.org/web-animations-1/#playback-direction>
    playback_direction: PlaybackDirection,

    /// <https://drafts.csswg.org/css-easing-2/#easing-function>
    #[no_trace]
    easing_function: TimingFunction,
}

impl AnimationEffect {
    pub(crate) fn new_inherited(window: &Window) -> Self {
        Self {
            reflector: Reflector::new(),
            window: Dom::from_ref(window),

            // The default values of the timing properties specified here don't matter.
            // There is no way to construct a AnimationEffect without subsequently initializing them,
            // even if they're not passed to the constructor.
            specified_timing_properties: DomRefCell::new(SpecifiedTimingProperties {
                start_delay: Default::default(),
                end_delay: Default::default(),
                fill_mode: FillMode::None,
                iteration_count: Default::default(),
                iteration_start: Default::default(),
                iteration_duration: IterationDurationOrAuto::Auto,
                playback_direction: PlaybackDirection::Normal,
                easing_function: TimingFunction::Keyword(TimingKeyword::Linear),
            }),
            animation: Default::default(),
        }
    }

    pub(crate) fn window(&self) -> &Window {
        &self.window
    }

    /// The animation this effect is associated with, if any.
    pub(crate) fn associated_animation(&self) -> Option<DomRoot<Animation>> {
        self.animation.get()
    }

    pub(crate) fn set_associated_animation(&self, animation: Option<&Animation>) {
        self.animation.set(animation);
    }

    /// <https://drafts.csswg.org/web-animations-1/#active-duration>
    pub(crate) fn active_duration(&self) -> f64 {
        let timing = self.specified_timing_properties.borrow();
        let duration = timing.iteration_duration.computed_value();
        if duration == 0.0 || timing.iteration_count == 0.0 {
            return 0.0;
        }
        duration * timing.iteration_count
    }

    /// <https://drafts.csswg.org/web-animations-1/#end-time>
    pub(crate) fn end_time(&self) -> f64 {
        let (start_delay, end_delay) = {
            let timing = self.specified_timing_properties.borrow();
            (*timing.start_delay, *timing.end_delay)
        };
        (start_delay + self.active_duration() + end_delay).max(0.0)
    }

    /// Runs the timing calculations of
    /// <https://drafts.csswg.org/web-animations-1/#core-animation-effect-model> for the
    /// associated animation's current time.
    pub(crate) fn calculate_timing(&self) -> EffectTimingState {
        let (local_time, playback_rate) = match self.animation.get() {
            Some(animation) => (animation.current_time(), animation.playback_rate()),
            None => (None, 1.0),
        };
        self.calculate_timing_for(local_time, playback_rate)
    }

    fn calculate_timing_for(
        &self,
        local_time: Option<f64>,
        playback_rate: f64,
    ) -> EffectTimingState {
        let active_duration = self.active_duration();
        let end_time = self.end_time();
        let timing = self.specified_timing_properties.borrow();
        let start_delay = *timing.start_delay;
        let iteration_duration = timing.iteration_duration.computed_value();
        let iteration_count = timing.iteration_count;
        let iteration_start = *timing.iteration_start;

        // <https://drafts.csswg.org/web-animations-1/#animation-effect-phases-and-states>
        let backwards = playback_rate < 0.0;
        let before_active_boundary = start_delay.min(end_time).max(0.0);
        let active_after_boundary = (start_delay + active_duration).min(end_time).max(0.0);
        let phase = match local_time {
            None => EffectPhase::Idle,
            Some(time)
                if time < before_active_boundary ||
                    (backwards && time == before_active_boundary) =>
            {
                EffectPhase::Before
            },
            Some(time)
                if time > active_after_boundary ||
                    (!backwards && time == active_after_boundary) =>
            {
                EffectPhase::After
            },
            Some(_) => EffectPhase::Active,
        };

        // <https://drafts.csswg.org/web-animations-1/#calculating-the-active-time>
        let fills_backwards = matches!(timing.fill_mode, FillMode::Backwards | FillMode::Both);
        let fills_forwards = matches!(timing.fill_mode, FillMode::Forwards | FillMode::Both);
        let active_time = match (phase, local_time) {
            (EffectPhase::Before, Some(time)) if fills_backwards => {
                Some((time - start_delay).max(0.0))
            },
            (EffectPhase::Active, Some(time)) => Some(time - start_delay),
            (EffectPhase::After, Some(time)) if fills_forwards => {
                Some((time - start_delay).min(active_duration).max(0.0))
            },
            _ => None,
        };

        // <https://drafts.csswg.org/web-animations-1/#calculating-the-overall-progress>
        let overall_progress = active_time.map(|active_time| {
            let progress = if iteration_duration == 0.0 {
                if phase == EffectPhase::Before {
                    0.0
                } else {
                    iteration_count
                }
            } else {
                active_time / iteration_duration
            };
            progress + iteration_start
        });

        // <https://drafts.csswg.org/web-animations-1/#calculating-the-simple-iteration-progress>
        let simple_iteration_progress = overall_progress.map(|overall_progress| {
            let mut progress = if overall_progress.is_infinite() {
                iteration_start % 1.0
            } else {
                overall_progress % 1.0
            };
            if progress == 0.0 &&
                matches!(phase, EffectPhase::Active | EffectPhase::After) &&
                active_time == Some(active_duration) &&
                iteration_count != 0.0
            {
                progress = 1.0;
            }
            progress
        });

        // <https://drafts.csswg.org/web-animations-1/#calculating-the-current-iteration>
        let current_iteration = match (active_time, overall_progress, simple_iteration_progress) {
            (Some(_), Some(overall), Some(simple)) => {
                if phase == EffectPhase::After && iteration_count.is_infinite() {
                    Some(f64::INFINITY)
                } else if simple == 1.0 {
                    Some(overall.floor() - 1.0)
                } else {
                    Some(overall.floor())
                }
            },
            _ => None,
        };

        // <https://drafts.csswg.org/web-animations-1/#calculating-the-directed-progress>
        let iteration = current_iteration.unwrap_or(0.0);
        let odd_iteration = iteration.is_finite() && iteration % 2.0 != 0.0;
        let current_direction_forwards = match timing.playback_direction {
            PlaybackDirection::Normal => true,
            PlaybackDirection::Reverse => false,
            PlaybackDirection::Alternate => !odd_iteration,
            PlaybackDirection::Alternate_reverse => odd_iteration,
        };
        let directed_progress = simple_iteration_progress.map(|progress| {
            if current_direction_forwards {
                progress
            } else {
                1.0 - progress
            }
        });

        // <https://drafts.csswg.org/web-animations-1/#calculating-the-transformed-progress>
        let transformed_progress = directed_progress.map(|progress| {
            let going_forwards = !backwards;
            let before_flag = if (phase == EffectPhase::Before && going_forwards) ||
                (phase == EffectPhase::After && !going_forwards)
            {
                BeforeFlag::Set
            } else {
                BeforeFlag::Unset
            };
            timing
                .easing_function
                .to_computed_value_without_context()
                .calculate_output(progress, before_flag, 1e-7)
        });

        EffectTimingState {
            local_time,
            phase,
            playback_rate,
            end_time,
            active_duration,
            current_iteration,
            transformed_progress,
            active_time,
            start_delay,
            progress_increasing: current_direction_forwards != backwards,
        }
    }

    /// Whether the playback direction alternates between iterations.
    pub(crate) fn alternates(&self) -> bool {
        matches!(
            self.specified_timing_properties.borrow().playback_direction,
            PlaybackDirection::Alternate | PlaybackDirection::Alternate_reverse
        )
    }

    /// <https://drafts.csswg.org/web-animations-1/#update-the-timing-properties-of-an-animation-effect>
    pub(crate) fn update_the_timing_properties(
        &self,
        input: &OptionalEffectTiming,
    ) -> Fallible<()> {
        // Step 1. If the iterationStart member of input exists and is less than zero,
        // throw a TypeError and abort this procedure.
        if input
            .iterationStart
            .is_some_and(|iteration_start| *iteration_start < 0.0)
        {
            return Err(Error::Type(
                c"Negative values for iterationStart are not allowed".to_owned(),
            ));
        }

        // Step 2. If the iterations member of input exists, and is less than zero or is the value NaN,
        // throw a TypeError and abort this procedure.
        if input
            .iterations
            .is_some_and(|iterations| iterations < 0.0 || iterations.is_nan())
        {
            return Err(Error::Type(
                c"\"iterations\" must be a positive number".to_owned(),
            ));
        }

        // Step 3. If the duration member of input exists, and is less than zero, is the value NaN,
        // or is a string other than the lowercase auto, throw a TypeError and abort this procedure.
        let Ok(duration) = input
            .duration
            .as_ref()
            .map(|duration| match duration {
                UnrestrictedDoubleOrString::UnrestrictedDouble(double) => {
                    if *double < 0.0 || double.is_nan() {
                        Err(())
                    } else {
                        Ok(IterationDurationOrAuto::Duration(*double))
                    }
                },
                UnrestrictedDoubleOrString::String(string) => {
                    if string == "auto" {
                        Ok(IterationDurationOrAuto::Auto)
                    } else {
                        Err(())
                    }
                },
            })
            .transpose()
        else {
            return Err(Error::Type(
                c"\"duration\" must be a positive number".to_owned(),
            ));
        };

        // Step 4. If the easing member of input exists but cannot be parsed using the <easing-function> production [CSS-EASING-1],
        // throw a TypeError and abort this procedure.
        let Ok(easing) = input
            .easing
            .as_ref()
            .map(|easing| {
                let easing = easing.str();
                let mut parser = Parser::new(&easing);

                // None of these values should matter
                let document = self.window.Document();
                let urlextradata = document.url().into_url().into();
                let parser_context = parser_context_for_document(
                    &document,
                    CssRuleType::Style,
                    ParsingMode::DEFAULT,
                    &urlextradata,
                );
                TimingFunction::parse(&parser_context, &mut parser).map_err(|_| ())
            })
            .transpose()
        else {
            return Err(Error::Type(
                c"\"easing\" is not a valid timing function".to_owned(),
            ));
        };

        // Step 5. Assign each member that exists in input to the corresponding timing property of effect as follows:
        // delay → start delay
        let mut specified_timing_properties = self.specified_timing_properties.borrow_mut();
        if let Some(start_delay) = input.delay {
            specified_timing_properties.start_delay = start_delay;
        }

        // endDelay → end delay
        if let Some(end_delay) = input.endDelay {
            specified_timing_properties.end_delay = end_delay;
        }

        // fill → fill mode
        if let Some(fill) = input.fill {
            specified_timing_properties.fill_mode = fill;
        }

        // iterationStart → iteration start
        if let Some(iteration_start) = input.iterationStart {
            specified_timing_properties.iteration_start = iteration_start;
        }

        // iterations → iteration count
        if let Some(iterations) = input.iterations {
            specified_timing_properties.iteration_count = iterations;
        }

        // duration → iteration duration
        if let Some(duration) = duration {
            specified_timing_properties.iteration_duration = duration;
        }

        // direction → playback direction
        if let Some(direction) = input.direction {
            specified_timing_properties.playback_direction = direction;
        }

        // easing → easing function
        if let Some(easing) = easing {
            specified_timing_properties.easing_function = easing;
        }
        Ok(())
    }
}

impl AnimationEffectMethods<crate::DomTypeHolder> for AnimationEffect {
    /// <https://drafts.csswg.org/web-animations-1/#dom-animationeffect-gettiming>
    fn GetTiming(&self) -> EffectTiming {
        // > Returns the specified timing properties for this animation effect.
        let specified_timing_properties = self.specified_timing_properties.borrow();
        EffectTiming {
            delay: specified_timing_properties.start_delay,
            direction: specified_timing_properties.playback_direction,
            duration: specified_timing_properties.iteration_duration.into(),
            easing: specified_timing_properties
                .easing_function
                .to_css_string()
                .into(),
            endDelay: specified_timing_properties.end_delay,
            fill: specified_timing_properties.fill_mode,
            iterationStart: specified_timing_properties.iteration_start,
            iterations: specified_timing_properties.iteration_count,
        }
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-animationeffect-getcomputedtiming>
    fn GetComputedTiming(&self) -> ComputedEffectTiming {
        // > Returns the calculated timing properties for this animation effect.
        if let Some(animation) = self.animation.get() {
            animation.refresh_timeline();
        }
        let state = self.calculate_timing();
        let specified_timing_properties = self.specified_timing_properties.borrow();

        // > while getTiming() can return the string auto, getComputedTiming() must return a number
        // > corresponding to the calculated value of the iteration duration as defined in the description
        // > of the duration member of the EffectTiming interface.
        // > In this level of the specification, that simply means that an auto value is replaced by zero.
        let computed_duration = specified_timing_properties
            .iteration_duration
            .computed_value();

        // > likewise, while getTiming() can return the string auto, getComputedTiming() must return the
        // > specific FillMode used for timing calculations as defined in the description of the fill
        // > member of the EffectTiming interface.
        // > In this level of the specification, that simply means that an auto value is replaced by the none FillMode.
        let computed_fill_mode = if specified_timing_properties.fill_mode == FillMode::Auto {
            FillMode::None
        } else {
            specified_timing_properties.fill_mode
        };

        ComputedEffectTiming {
            parent: EffectTiming {
                delay: specified_timing_properties.start_delay,
                direction: specified_timing_properties.playback_direction,
                duration: UnrestrictedDoubleOrString::UnrestrictedDouble(computed_duration),
                easing: specified_timing_properties
                    .easing_function
                    .to_css_string()
                    .into(),
                endDelay: specified_timing_properties.end_delay,
                fill: computed_fill_mode,
                iterationStart: specified_timing_properties.iteration_start,
                iterations: specified_timing_properties.iteration_count,
            },
            endTime: Some(state.end_time),
            activeDuration: Some(state.active_duration),
            localTime: Some(state.local_time.and_then(Finite::new)),
            progress: Some(state.transformed_progress.and_then(Finite::new)),
            currentIteration: Some(state.current_iteration),
        }
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-animationeffect-updatetiming>
    fn UpdateTiming(&self, timing: &OptionalEffectTiming) -> Fallible<()> {
        // > Updates the specified timing properties of this animation effect by performing the procedure
        // > to update the timing properties of an animation effect passing the timing parameter as input.
        self.update_the_timing_properties(timing)
    }
}

#[derive(Clone, Copy, Debug, JSTraceable, MallocSizeOf)]
enum IterationDurationOrAuto {
    Duration(f64),
    Auto,
}

impl IterationDurationOrAuto {
    fn computed_value(&self) -> f64 {
        match self {
            IterationDurationOrAuto::Auto => 0.0,
            IterationDurationOrAuto::Duration(double) => *double,
        }
    }
}

impl From<IterationDurationOrAuto> for UnrestrictedDoubleOrString {
    fn from(value: IterationDurationOrAuto) -> Self {
        match value {
            IterationDurationOrAuto::Auto => {
                UnrestrictedDoubleOrString::String(DOMString::from_static("auto"))
            },
            IterationDurationOrAuto::Duration(double) => {
                UnrestrictedDoubleOrString::UnrestrictedDouble(double)
            },
        }
    }
}

/// <https://drafts.csswg.org/web-animations-1/#animation-effect-phases-and-states>
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum EffectPhase {
    Before,
    Active,
    After,
    Idle,
}

/// The results of the timing calculations for an animation effect at one moment.
pub(crate) struct EffectTimingState {
    pub(crate) local_time: Option<f64>,
    pub(crate) phase: EffectPhase,
    pub(crate) playback_rate: f64,
    pub(crate) end_time: f64,
    pub(crate) active_duration: f64,
    pub(crate) active_time: Option<f64>,
    pub(crate) current_iteration: Option<f64>,
    /// <https://drafts.csswg.org/web-animations-1/#transformed-progress>
    pub(crate) transformed_progress: Option<f64>,
    /// <https://drafts.csswg.org/web-animations-1/#start-delay>
    pub(crate) start_delay: f64,
    /// Whether the directed progress grows as time passes (playback rate and direction).
    pub(crate) progress_increasing: bool,
}

impl EffectTimingState {
    /// <https://drafts.csswg.org/web-animations-1/#in-play>
    pub(crate) fn is_current(&self) -> bool {
        match self.phase {
            EffectPhase::Active => true,
            EffectPhase::Before => self.playback_rate > 0.0,
            EffectPhase::After => self.playback_rate < 0.0,
            EffectPhase::Idle => false,
        }
    }

    /// <https://drafts.csswg.org/web-animations-1/#in-effect>
    pub(crate) fn is_in_effect(&self) -> bool {
        self.active_time.is_some()
    }
}
