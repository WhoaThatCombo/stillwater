/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::Cell;
use std::ffi::CString;
use std::ops::ControlFlow;
use std::ptr::NonNull;
use std::sync::LazyLock;

use cssparser::Parser;
use dom_struct::dom_struct;
use js::context::JSContext;
use js::conversions::{
    ConversionResult, FromJSValConvertible, ToJSValConvertible, jsstr_to_string,
};
use js::gc::{HandleValue, RootedVec};
use js::jsapi::{Heap, JS_GetPropertyById, JSITER_OWNONLY, JSObject};
use js::jsval::{ObjectValue, UndefinedValue};
use js::rust::{
    ForOfIterationFailure, HandleObject, IdVector, IntoHandle, IntoMutableHandle, for_of,
};
use rustc_hash::FxHashMap;
use script_bindings::cell::DomRefCell;
use script_bindings::codegen::GenericBindings::AnimationEffectBinding::{
    EffectTiming, OptionalEffectTiming,
};
use script_bindings::codegen::GenericBindings::CSSStyleDeclarationBinding::CSSStyleDeclarationMethods;
use script_bindings::codegen::GenericBindings::HTMLElementBinding::HTMLElementMethods;
use script_bindings::codegen::GenericBindings::KeyframeEffectBinding::{
    BaseKeyframe, CompositeOperation, CompositeOperationOrAuto,
};
use script_bindings::codegen::GenericBindings::SVGElementBinding::SVGElementMethods;
use script_bindings::codegen::GenericBindings::WindowBinding::WindowMethods;
use script_bindings::codegen::GenericUnionTypes::{
    UnrestrictedDoubleOrKeyframeEffectOptions, UnrestrictedDoubleOrString,
};
use script_bindings::conversions::StringificationBehavior;
use script_bindings::error::{Error, Fallible};
use script_bindings::inheritance::Castable;
use script_bindings::js::{
    GetPropertyKeys, JS_DefineProperty, JS_IdToValue, JS_NewObject, PropertyFlags,
};
use script_bindings::num::Finite;
use script_bindings::reflector::reflect_dom_object_with_proto;
use script_bindings::root::DomRoot;
use script_bindings::str::DOMString;
use style::parser::ParserContext;
use style::properties::generated::PropertyDeclaration;
use style::properties::{
    Importance, LonghandId, NonCustomPropertyId, PropertyDeclarationBlock, PropertyId,
    SourcePropertyDeclaration,
};
use style::stylesheets::CssRuleType;
use style_traits::{CssWriter, ParsingMode, ToCss};

use crate::css::css::parser_context_for_document;
use crate::dom::Document;
use crate::dom::animationeffect::{AnimationEffect, EffectPhase};
use crate::dom::bindings::codegen::Bindings::AnimationBinding::AnimationMethods;
use crate::dom::bindings::codegen::Bindings::DocumentBinding::DocumentMethods;
use crate::dom::bindings::codegen::Bindings::KeyframeEffectBinding::{
    BaseComputedKeyframe, KeyframeEffectMethods,
};
use crate::dom::bindings::root::MutNullableDom;
use crate::dom::element::Element;
use crate::dom::html::htmlelement::HTMLElement;
use crate::dom::node::{Node, NodeDamage, NodeTraits};
use crate::dom::svg::svgelement::SVGElement;
use crate::dom::window::Window;

/// <https://drafts.csswg.org/web-animations-1/#keyframeeffect>
#[dom_struct]
pub(crate) struct KeyframeEffect {
    animationeffect: AnimationEffect,

    /// <https://drafts.csswg.org/web-animations-1/#effect-target-target-element>
    // FIXME: Store a target pseudo-selector
    // to fully match the concept of the effect target
    //
    // https://drafts.csswg.org/web-animations-1/#effect-target-target-pseudo-selector
    // https://drafts.csswg.org/web-animations-1/#keyframe-effect-effect-target.
    target_element: MutNullableDom<Element>,

    /// <https://drafts.csswg.org/web-animations-1/#keyframe>
    keyframes: DomRefCell<Vec<Keyframe>>,

    /// <https://drafts.csswg.org/web-animations-1/#effect-composite-operation>
    composite: Cell<CompositeOperation>,

    /// The keyframe whose values this effect currently applies to its target, if any.
    applied_keyframe: Cell<Option<usize>>,
}

impl KeyframeEffect {
    pub(crate) fn new_inherited(window: &Window) -> Self {
        Self {
            animationeffect: AnimationEffect::new_inherited(window),
            target_element: Default::default(),
            keyframes: Default::default(),
            composite: Cell::new(CompositeOperation::Replace),
            applied_keyframe: Cell::new(None),
        }
    }

    fn new_with_proto(
        cx: &mut JSContext,
        window: &Window,
        proto: Option<HandleObject>,
    ) -> DomRoot<Self> {
        reflect_dom_object_with_proto(cx, Box::new(Self::new_inherited(window)), window, proto)
    }

    pub(crate) fn new(cx: &mut JSContext, window: &Window) -> DomRoot<Self> {
        Self::new_with_proto(cx, window, None)
    }

    /// The target element of this effect, if any.
    pub(crate) fn target(&self) -> Option<DomRoot<Element>> {
        self.target_element.get()
    }

    /// The keyframe whose values the effect applies at its current progress, if it is in
    /// effect. Stillwater doesn't interpolate script animations: during the active phase the
    /// effect shows the keyframe it is heading towards (so each segment snaps to its end
    /// state), and in the fill phases it shows the keyframe at that exact progress.
    pub(crate) fn effect_keyframe_index(&self) -> Option<usize> {
        let state = self.upcast::<AnimationEffect>().calculate_timing();
        let progress = state.transformed_progress?;
        let keyframes = self.keyframes.borrow();
        if keyframes.is_empty() {
            return None;
        }
        let offsets = compute_missing_keyframe_offsets(&keyframes);
        let last = offsets.len() - 1;
        if state.phase == EffectPhase::Active {
            if state.progress_increasing {
                Some(
                    offsets
                        .iter()
                        .position(|offset| *offset > progress)
                        .unwrap_or(last),
                )
            } else {
                Some(
                    offsets
                        .iter()
                        .rposition(|offset| *offset < progress)
                        .unwrap_or(0),
                )
            }
        } else {
            Some(
                offsets
                    .iter()
                    .rposition(|offset| *offset <= progress)
                    .unwrap_or(0),
            )
        }
    }

    /// Whether the applied keyframe can change during the active phase, so the animation has
    /// to check it periodically.
    pub(crate) fn needs_periodic_updates(&self) -> bool {
        self.keyframes.borrow().len() > 2 || self.upcast::<AnimationEffect>().alternates()
    }

    /// Record which keyframe is applied; returns whether that changed.
    pub(crate) fn set_applied_keyframe(&self, index: Option<usize>) -> bool {
        self.applied_keyframe.replace(index) != index
    }

    /// Forget the applied keyframe, so the next update reapplies it.
    pub(crate) fn invalidate_applied_keyframe(&self) {
        self.applied_keyframe.set(Some(usize::MAX));
    }

    /// The declarations of the keyframe this effect currently applies.
    pub(crate) fn applied_declarations(&self) -> Vec<PropertyDeclaration> {
        let Some(index) = self.applied_keyframe.get() else {
            return Vec::new();
        };
        let keyframes = self.keyframes.borrow();
        let Some(keyframe) = keyframes.get(index) else {
            return Vec::new();
        };
        keyframe
            .declarations
            .iter()
            .flat_map(|pair| {
                pair.block
                    .declaration_importance_iter()
                    .map(|(declaration, _)| declaration.clone())
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-animation-commitstyles>
    ///
    /// Stillwater doesn't interpolate script animations, so this commits the keyframe that is
    /// closest to the effect's current progress (exact at the start and end of an animation,
    /// which is where pages call it).
    pub(crate) fn commit_styles(&self, cx: &mut JSContext) -> Fallible<()> {
        // Step 1. Let targets be the set of all effect targets for animation effects associated
        // with animation.
        // Step 2. For each target in targets:
        let Some(target) = self.target_element.get() else {
            return Ok(());
        };
        // Step 2.1 If target is not an element capable of having a style attribute (for
        // example, it is a pseudo-element or is an element in a document format for which
        // style attributes are not defined) throw a "NoModificationAllowedError" DOMException
        // and abort these steps.
        let style = if let Some(element) = target.downcast::<HTMLElement>() {
            element.Style(cx)
        } else if let Some(element) = target.downcast::<SVGElement>() {
            element.Style(cx)
        } else {
            return Err(Error::NoModificationAllowed(None));
        };

        let Some(progress) = self
            .upcast::<AnimationEffect>()
            .calculate_timing()
            .transformed_progress
        else {
            return Ok(());
        };
        let declarations = {
            let keyframes = self.keyframes.borrow();
            let offsets = compute_missing_keyframe_offsets(&keyframes);
            let Some(index) = (0..keyframes.len()).min_by(|a, b| {
                let distance_a = (offsets[*a] - progress).abs();
                let distance_b = (offsets[*b] - progress).abs();
                distance_a.total_cmp(&distance_b).then(b.cmp(a))
            }) else {
                return Ok(());
            };
            self.serialize_declarations(&keyframes[index])
        };

        // Step 2.5 For each property, property, in targeted properties: ... Set a CSS
        // declaration property for declaration with the property name and value.
        for (name, value) in declarations {
            style.SetProperty(cx, name.into(), value.into(), DOMString::new())?;
        }
        Ok(())
    }

    /// The (property name, value) pairs of a keyframe, serialized.
    fn serialize_declarations(&self, keyframe: &Keyframe) -> Vec<(String, String)> {
        let window = self.upcast::<AnimationEffect>().window();
        keyframe
            .declarations
            .iter()
            .filter_map(|property_value_pair| {
                let mut property_name = String::new();
                let mut writer = CssWriter::new(&mut property_name);
                property_value_pair.property_id.to_css(&mut writer).ok()?;
                let mut value_string = String::new();
                let mut layout = window.layout_mut();
                property_value_pair
                    .block
                    .single_value_to_css(
                        &property_value_pair.property_id,
                        &mut value_string,
                        None,
                        layout.stylist_mut(),
                    )
                    .ok()?;
                Some((property_name, value_string))
            })
            .collect()
    }
}

impl KeyframeEffectMethods<crate::DomTypeHolder> for KeyframeEffect {
    /// <https://drafts.csswg.org/web-animations-1/#dom-keyframeeffect-keyframeeffect>
    fn Constructor(
        cx: &mut JSContext,
        window: &Window,
        proto: Option<HandleObject>,
        target: Option<&Element>,
        keyframes: *mut JSObject,
        options: UnrestrictedDoubleOrKeyframeEffectOptions,
    ) -> Fallible<DomRoot<KeyframeEffect>> {
        // Step 1. Create a new KeyframeEffect object, effect.
        let effect = KeyframeEffect::new_with_proto(cx, window, proto);

        // Step 2. Set the target element of effect to target.
        effect.target_element.set(target);

        // TODO: Step 3. Set the target pseudo-selector to the result corresponding to
        // the first matching condition below:

        // Step 4. Let timing input be the result corresponding to the first matching
        // condition below:
        let default_timing;
        let (timing, duration_override) = match &options {
            // If options is a KeyframeEffectOptions object, Let timing input be options.
            UnrestrictedDoubleOrKeyframeEffectOptions::KeyframeEffectOptions(options) => {
                (&options.parent, None)
            },
            // Otherwise (if options is a double), Let timing input be a new EffectTiming
            // object with all members set to their default values and duration set to
            // options.
            UnrestrictedDoubleOrKeyframeEffectOptions::UnrestrictedDouble(duration) => {
                default_timing = EffectTiming::empty();
                (&default_timing, Some(*duration))
            },
        };
        let mut timing_input = OptionalEffectTiming::empty();
        timing_input.delay = Some(timing.delay);
        timing_input.direction = Some(timing.direction);
        timing_input.duration = Some(match (duration_override, &timing.duration) {
            (Some(duration), _) => UnrestrictedDoubleOrString::UnrestrictedDouble(duration),
            (None, UnrestrictedDoubleOrString::UnrestrictedDouble(duration)) => {
                UnrestrictedDoubleOrString::UnrestrictedDouble(*duration)
            },
            (None, UnrestrictedDoubleOrString::String(duration)) => {
                UnrestrictedDoubleOrString::String(duration.clone())
            },
        });
        timing_input.easing = Some(timing.easing.clone());
        timing_input.endDelay = Some(timing.endDelay);
        timing_input.fill = Some(timing.fill);
        timing_input.iterationStart = Some(timing.iterationStart);
        timing_input.iterations = Some(timing.iterations);

        // Step 5. Call the procedure to update the timing properties of an animation effect of
        // effect from timing input.
        // If that procedure causes an exception to be thrown, propagate the exception and abort this procedure.
        effect
            .upcast::<AnimationEffect>()
            .update_the_timing_properties(&timing_input)?;

        // Step 6. If options is a KeyframeEffectOptions object, assign the composite property of effect
        // to the corresponding value from options.
        if let UnrestrictedDoubleOrKeyframeEffectOptions::KeyframeEffectOptions(options) = &options
        {
            effect.composite.set(options.composite);
        }

        // Step 7. Initialize the set of keyframes by performing the procedure defined for setKeyframes()
        // passing keyframes as the input.
        effect.SetKeyframes(cx, keyframes)?;

        Ok(effect)
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-keyframeeffect-target>
    fn GetTarget(&self) -> Option<DomRoot<Element>> {
        self.target_element.get()
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-keyframeeffect-target>
    fn SetTarget(&self, cx: &mut JSContext, target: Option<&Element>) {
        let old_target = self.target_element.get();
        self.target_element.set(target);
        if let Some(animation) = self.upcast::<AnimationEffect>().associated_animation() {
            self.invalidate_applied_keyframe();
            animation.update_effect_values(cx);
            if let Some(old_target) = old_target {
                apply_script_animations_to(cx, &old_target);
            }
        }
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-keyframeeffect-composite>
    fn Composite(&self) -> CompositeOperation {
        self.composite.get()
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-keyframeeffect-composite>
    fn SetComposite(&self, composite: CompositeOperation) {
        self.composite.set(composite);
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-keyframeeffect-getkeyframes>
    fn GetKeyframes(
        &self,
        cx: &mut JSContext,
        result: &mut RootedVec<'_, Box<Heap<*mut JSObject>>>,
    ) -> Fallible<()> {
        let window = self.upcast::<AnimationEffect>().window();

        // Step 1. Let result be an empty sequence of objects.
        debug_assert!(result.is_empty());

        // Step 2. Let keyframes be one of the following:
        // If this keyframe effect is associated with a CSSAnimation, and its keyframes have not been replaced
        // by a successful call to setKeyframes(),
        // the computed keyframes for this keyframe effect.
        // Otherwise,
        // the result of applying the procedure compute missing keyframe offsets to the keyframes for this keyframe effect.
        let keyframes = self.keyframes.borrow();
        let computed_offsets = compute_missing_keyframe_offsets(&keyframes);

        // Step 3. For each keyframe in keyframes perform the following steps:
        for (keyframe, computed_offset) in keyframes.iter().zip(computed_offsets) {
            // Step 3.1 Initialize a dictionary object, output keyframe, using the following definition:
            // Step 3.2 Set the offset, computedOffset, easing, and composite members of output keyframe
            // to the respective keyframe offset, computed keyframe offset, keyframe-specific easing function,
            // and keyframe-specific composite operation values of keyframe.
            let base_keyframe = BaseComputedKeyframe {
                composite: keyframe.composite,
                offset: keyframe.offset,
                computedOffset: Some(Finite::wrap(computed_offset)),
                easing: keyframe.easing_function.clone(),
            };
            rooted!(&in(cx) let mut output_keyframe = JS_NewObject(cx, None));
            base_keyframe.to_jsobject(cx, output_keyframe.handle_mut());

            // Step 3.3 For each animation property-value pair declaration in keyframe, perform the following steps:
            for property_value_pair in &keyframe.declarations {
                debug_assert!(property_value_pair.property_id.is_animatable());

                // Step 3.3.1 Let property name be the result of applying the animation property name to IDL attribute
                // name algorithm to the property name of declaration.
                let mut property_name = String::new();
                let mut writer = CssWriter::new(&mut property_name);
                if property_value_pair.property_id.to_css(&mut writer).is_err() {
                    continue;
                }
                let property_name = animation_property_name_to_idl_attribute_name(&property_name);

                // Step 3.3.2 Let IDL value be the result of serializing the property value of declaration
                // by passing declaration to the algorithm to serialize a CSS value [CSSOM].
                let mut value_string = String::new();
                let is_single_value_value_to_css = {
                    let mut layout = window.layout_mut();
                    property_value_pair.block.single_value_to_css(
                        &property_value_pair.property_id,
                        &mut value_string,
                        None,
                        layout.stylist_mut(),
                    )
                };
                if is_single_value_value_to_css.is_err() {
                    continue;
                }

                // Step 3.3.3 Let value be the result of converting IDL value to an ECMAScript String value.
                rooted!(&in(cx) let mut value = UndefinedValue());
                value_string.to_jsval(cx, value.handle_mut());

                // Step 3.3.4 Call the [[DefineOwnProperty]] internal method on output keyframe with property
                // name property name, Property Descriptor { [[Writable]]: true, [[Enumerable]]: true, [[Configurable]]:
                // true, [[Value]]: value } and Boolean flag false.
                let Ok(property_name) = CString::new(property_name) else {
                    continue;
                };

                let success = JS_DefineProperty(
                    cx,
                    output_keyframe.handle(),
                    &property_name,
                    value.handle(),
                    PropertyFlags::ENUMERABLE |
                        PropertyFlags::WRITABLE |
                        PropertyFlags::CONFIGURABLE,
                );
                if !success {
                    if cfg!(debug_assertions) {
                        unreachable!("Setting a property on output_keyframe should never fail");
                    }
                    return Err(Error::Operation(None));
                }
            }

            // Step 3.4 Append output keyframe to result.
            result.push(Heap::boxed(output_keyframe.get()))
        }

        // Step 4. Return result.
        Ok(())
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-keyframeeffect-setkeyframes>
    fn SetKeyframes(&self, cx: &mut JSContext, keyframes: *mut JSObject) -> Fallible<()> {
        // > This effect's set of keyframes is replaced with the result of performing the procedure to
        // > process a keyframes argument. If that procedure throws an exception, this effect's
        // > keyframes are not modified.
        let document = self.upcast::<AnimationEffect>().window().Document();
        let keyframes = process_a_keyframes_argument(cx, &document, keyframes)?;
        *self.keyframes.safe_borrow_mut(cx.no_gc()) = keyframes;
        if let Some(animation) = self.upcast::<AnimationEffect>().associated_animation() {
            self.invalidate_applied_keyframe();
            animation.update_effect_values(cx);
        }
        Ok(())
    }
}

/// Recompute the declarations that script-created animations apply to `target` (in
/// composite order) and restyle it.
pub(crate) fn apply_script_animations_to(cx: &mut JSContext, target: &Element) {
    let document = target.owner_document();
    let mut block = PropertyDeclarationBlock::new();
    for animation in document.Timeline().animations() {
        let Some(effect) = animation
            .GetEffect()
            .and_then(DomRoot::downcast::<KeyframeEffect>)
        else {
            continue;
        };
        if effect.target_element.get().as_deref() != Some(target) {
            continue;
        }
        for declaration in effect.applied_declarations() {
            block.push(declaration, Importance::Normal);
        }
    }
    let declarations = if block.is_empty() {
        None
    } else {
        Some(servo_arc::Arc::new(block))
    };
    let node = target.upcast::<Node>();
    document
        .animation_manager()
        .set_script_animation_declarations(node, declarations);
    node.dirty(cx.no_gc(), NodeDamage::Style);
}

/// <https://drafts.csswg.org/web-animations-1/#compute-missing-keyframe-offsets>
fn compute_missing_keyframe_offsets(keyframes: &[Keyframe]) -> Vec<f64> {
    // Step 1. For each keyframe, in keyframes, let the computed keyframe offset of the
    // keyframe be equal to its keyframe offset value.
    let mut offsets: Vec<Option<f64>> = keyframes
        .iter()
        .map(|keyframe| keyframe.offset.map(|offset| *offset))
        .collect();

    // Step 2. If keyframes contains more than one keyframe and the computed keyframe offset of
    // the first keyframe in keyframes is null, set the computed keyframe offset of the first
    // keyframe to 0.
    if offsets.len() > 1 && offsets[0].is_none() {
        offsets[0] = Some(0.0);
    }

    // Step 3. If the computed keyframe offset of the last keyframe in keyframes is null, set
    // its computed keyframe offset to 1.
    if let Some(last) = offsets.last_mut() &&
        last.is_none()
    {
        *last = Some(1.0);
    }

    // Step 4. For each pair of keyframes A and B where A appears before B in keyframes, and A
    // and B have a computed keyframe offset that is not null, and all keyframes between A and
    // B have a null computed keyframe offset, calculate the computed keyframe offset of each
    // keyframe between A and B as follows: offset(k) = offset(A) + (offset(B) - offset(A)) × k / n
    let mut previous_defined = 0;
    for index in 1..offsets.len() {
        let Some(offset_b) = offsets[index] else {
            continue;
        };
        let offset_a = offsets[previous_defined].unwrap_or(0.0);
        let n = (index - previous_defined) as f64;
        for between in (previous_defined + 1)..index {
            let k = (between - previous_defined) as f64;
            offsets[between] = Some(offset_a + (offset_b - offset_a) * k / n);
        }
        previous_defined = index;
    }

    offsets
        .into_iter()
        .map(|offset| offset.unwrap_or(0.0))
        .collect()
}

/// <https://drafts.csswg.org/web-animations-1/#process-a-keyframes-argument>
fn process_a_keyframes_argument(
    cx: &mut JSContext,
    document: &Document,
    keyframes_object: *mut JSObject,
) -> Fallible<Vec<Keyframe>> {
    // Step 1. If object is null, return an empty sequence of keyframes.
    if keyframes_object.is_null() {
        return Ok(Vec::new());
    }

    // Step 2. Let processed keyframes be an empty sequence of keyframes.

    // Step 3. Let method be the result of GetMethod(object, @@iterator).
    // Step 4. Check the completion record of method.
    // Step 5. Perform the steps corresponding to the first matching condition below:
    rooted!(&in(cx) let iterable = ObjectValue(keyframes_object));
    let mut keyframes = Vec::new();
    let result = for_of(cx, iterable.handle(), |cx, iterator_element| {
        // Step 5.3.4 Let nextItem be IteratorValue(next).
        // Step 5.3.5 Check the completion record of nextItem.
        // Note: This happens inside the "for_of" call.

        // Step 5.3.6 If Type(nextItem) is not Undefined, Null or Object, then throw a TypeError
        // and abort these steps.
        if !iterator_element.is_null_or_undefined() && !iterator_element.is_object() {
            return Err(ForOfIterationFailure::Other(Error::Type(
                c"Keyframe must be an object, null or undefined".to_owned(),
            )));
        }

        // Step 5.3.7 Append to processed keyframes the result of running the procedure to process a
        // keyframe-like object passing nextItem as the keyframe input with the allow lists flag set to false.
        keyframes.push(process_a_keyframe_like_object(
            cx,
            document,
            iterator_element,
        )?);

        Ok(ControlFlow::Continue(()))
    });
    match result {
        Ok(()) => Ok(keyframes),
        Err(ForOfIterationFailure::ValueIsNotIterable) => {
            // Step 5, Otherwise: the object holds property-indexed keyframes.
            process_property_indexed_keyframes(cx, document, iterable.handle())
        },
        Err(ForOfIterationFailure::JSFailed) => Err(Error::JSFailed),
        Err(ForOfIterationFailure::Other(error)) => Err(error),
    }
}

/// The "Otherwise" branch of step 5 of
/// <https://drafts.csswg.org/web-animations-1/#process-a-keyframes-argument>:
/// `{ opacity: [0, 1], transform: "scale(2)", easing: "ease-in" }`.
///
/// Per-keyframe lists of `offset`, `easing` and `composite` aren't supported; a single
/// `easing` string applies to every keyframe.
fn process_property_indexed_keyframes(
    cx: &mut JSContext,
    document: &Document,
    object: HandleValue,
) -> Fallible<Vec<Keyframe>> {
    let urlextradata = document.url().into_url().into();
    let mut parser_context = parser_context_for_document(
        document,
        CssRuleType::Style,
        ParsingMode::DEFAULT,
        &urlextradata,
    );
    rooted!(&in(cx) let object = object.to_object());

    // Step 5.1 Let property-indexed keyframe be the result of running the procedure to process
    // a keyframe-like object passing object as the keyframe input and with the allow lists
    // flag set to true.
    let mut easing = DOMString::from_static("linear");
    let mut property_values: Vec<(NonCustomPropertyId, Vec<DOMString>)> = Vec::new();
    for_each_animatable_property(cx, object.handle(), |cx, name, value| {
        if name == "easing" {
            if let Ok(ConversionResult::Success(value)) =
                DOMString::from_jsval(cx, value, StringificationBehavior::Default)
            {
                easing = value;
            }
            return Ok(());
        }
        let Some(property_id) = lookup_css_property_by_idl_attribute_name(name) else {
            return Ok(());
        };
        // A sequence of values, or a single one.
        let mut values = Vec::new();
        if value.is_object() {
            let result = for_of(cx, value, |cx, item| {
                values.push(convert_to_domstring(cx, item)?);
                Ok(ControlFlow::Continue(()))
            });
            match result {
                Ok(()) => {},
                Err(ForOfIterationFailure::ValueIsNotIterable) => {
                    values.push(convert_to_domstring(cx, value)?);
                },
                Err(ForOfIterationFailure::JSFailed) => return Err(Error::JSFailed),
                Err(ForOfIterationFailure::Other(error)) => return Err(error),
            }
        } else {
            values.push(convert_to_domstring(cx, value)?);
        }
        property_values.push((property_id, values));
        Ok(())
    })?;

    // Step 5.2 Let processed keyframes be an empty sequence of keyframes.
    // Step 5.4 For each property-value pair in property-indexed keyframe, ... let property
    // keyframes be a keyframe for each value, and apply the procedure to compute missing
    // keyframe offsets to property keyframes. Add keyframes in property keyframes to processed
    // keyframes.
    let mut entries: Vec<(f64, KeyframePropertyDeclaration)> = Vec::new();
    for (property_id, values) in property_values {
        let count = values.len();
        for (index, value) in values.iter().enumerate() {
            let offset = if count == 1 {
                1.0
            } else {
                index as f64 / (count - 1) as f64
            };
            if let Some(declaration) =
                parse_single_property_declaration(property_id, &value.str(), &mut parser_context)
            {
                entries.push((offset, declaration));
            }
        }
    }

    // Step 5.5 Sort processed keyframes by the computed keyframe offset of each keyframe in
    // increasing order.
    // Step 5.6 Merge adjacent keyframes in processed keyframes when they have equal computed
    // keyframe offsets.
    entries.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut keyframes: Vec<Keyframe> = Vec::new();
    for (offset, declaration) in entries {
        match keyframes.last_mut() {
            Some(keyframe) if keyframe.offset.is_some_and(|existing| *existing == offset) => {
                keyframe.declarations.push(declaration);
            },
            _ => keyframes.push(Keyframe {
                offset: Finite::new(offset),
                easing_function: easing.clone(),
                composite: CompositeOperationOrAuto::Auto,
                declarations: vec![declaration],
            }),
        }
    }
    Ok(keyframes)
}

fn convert_to_domstring(cx: &mut JSContext, value: HandleValue) -> Result<DOMString, Error> {
    match DOMString::from_jsval(cx, value, StringificationBehavior::Default) {
        Ok(ConversionResult::Success(value)) => Ok(value),
        Ok(ConversionResult::Failure(_)) => Err(Error::Type(c"Invalid keyframe value".to_owned())),
        Err(_) => Err(Error::JSFailed),
    }
}

/// Calls `callback` with the name and value of each own string-keyed property of `object`.
#[expect(unsafe_code)]
fn for_each_animatable_property(
    cx: &mut JSContext,
    object: HandleObject,
    mut callback: impl FnMut(&mut JSContext, &str, HandleValue) -> Fallible<()>,
) -> Fallible<()> {
    let mut ids = IdVector::new(cx);
    if !GetPropertyKeys(cx, object, JSITER_OWNONLY, ids.handle_mut()) {
        return Err(Error::JSFailed);
    }
    for id in ids.iter() {
        rooted!(&in(cx) let id = *id);
        if !id.is_string() {
            continue;
        }
        rooted!(&in(cx) let mut key_value = UndefinedValue());
        if !JS_IdToValue(cx, id.handle().get(), key_value.handle_mut()) {
            continue;
        }
        rooted!(&in(cx) let js_string = key_value.to_string());
        let Some(js_string) = NonNull::new(js_string.get()) else {
            continue;
        };
        let property_name = unsafe { jsstr_to_string(cx, js_string) };
        rooted!(&in(cx) let mut property_value = UndefinedValue());
        if !unsafe {
            JS_GetPropertyById(
                cx.raw_cx(),
                object.into_handle(),
                id.handle().into_handle(),
                property_value.handle_mut().into_handle_mut(),
            )
        } {
            return Err(Error::JSFailed);
        }
        callback(cx, &property_name, property_value.handle())?;
    }
    Ok(())
}

/// <https://drafts.csswg.org/web-animations-1/#keyframe>
#[derive(JSTraceable, MallocSizeOf)]
struct Keyframe {
    offset: Option<Finite<f64>>,
    easing_function: DOMString,
    composite: CompositeOperationOrAuto,
    declarations: Vec<KeyframePropertyDeclaration>,
}

#[derive(JSTraceable, MallocSizeOf)]
struct KeyframePropertyDeclaration {
    #[no_trace]
    property_id: PropertyId,
    /// The block is known to only contain declarations for `property_id`. There might
    /// be more than one value if `property_id` is a shorthand.
    #[no_trace]
    block: PropertyDeclarationBlock,
}

/// <https://drafts.csswg.org/web-animations-1/#process-a-keyframe-like-object>
fn process_a_keyframe_like_object(
    cx: &mut JSContext,
    document: &Document,
    keyframe_input: HandleValue,
) -> Fallible<Keyframe> {
    // Step 1. Run the procedure to convert an ECMAScript value to a dictionary type [WEBIDL] with keyframe input
    // as the ECMAScript value, and the dictionary type depending on the value of the allow lists flag as follows:
    // If allow lists is true,
    // Use the following dictionary type:
    //
    // dictionary BasePropertyIndexedKeyframe {
    //   (double? or sequence<double?>)                         offset = [];
    //   (DOMString or sequence<DOMString>)                     easing = [];
    //   (CompositeOperationOrAuto or sequence<CompositeOperationOrAuto>) composite = [];
    // };
    //
    // Otherwise,
    //     Use the following dictionary type:
    //
    //     dictionary BaseKeyframe {
    //       double?                  offset = null;
    //       DOMString                easing = "linear";
    //       CompositeOperationOrAuto composite = "auto";
    //     };
    //
    // Store the result of this procedure as keyframe output.
    //
    // Note: 'allow lists' is currently never true.
    // Use the following dictionary type:
    let Ok(keyframe_output) = BaseKeyframe::from_jsval(cx, keyframe_input, ()) else {
        return Err(Error::JSFailed);
    };
    let ConversionResult::Success(keyframe_output) = keyframe_output else {
        return Err(Error::Type(c"Invalid keyframe".to_owned()));
    };
    let mut keyframe_output = Keyframe {
        offset: keyframe_output.offset,
        easing_function: keyframe_output.easing,
        composite: keyframe_output.composite,
        declarations: Vec::new(),
    };

    // Step 2. If keyframe input is null or undefined, return keyframe output.
    if keyframe_input.is_null_or_undefined() {
        return Ok(keyframe_output);
    }

    // From Step 3 onwards our implementation diverges from the specification. The spec
    // wants us to build a list of animatable CSS properties and a list of properties on
    // the object, then compute the union between the two.
    //
    // Instead, we iterate over all properties on the object and then check if they correspond
    // to an animatable property.
    let urlextradata = document.url().into_url().into();
    let mut parser_context = parser_context_for_document(
        document,
        CssRuleType::Style,
        ParsingMode::DEFAULT,
        &urlextradata,
    );
    rooted!(&in(cx) let object = keyframe_input.to_object());

    // Steps 2 - 6 are in get_property_declarations
    keyframe_output.declarations =
        get_property_declarations(cx, object.handle(), &mut parser_context)?;

    // Step 7. Return keyframe output.
    Ok(keyframe_output)
}

/// Implements Step 2-6 of  <https://drafts.csswg.org/web-animations-1/#process-a-keyframe-like-object>.
#[expect(unsafe_code)]
fn get_property_declarations(
    cx: &mut JSContext,
    object: HandleObject,
    parser_context: &mut ParserContext<'_>,
) -> Fallible<Vec<KeyframePropertyDeclaration>> {
    // The spec tells us to iterate over all animatable properties and see if they're defined
    // on the object. Instead we can iterate over the own properties of the object and see
    // if they're animated properties, that's easier.
    let mut ids = IdVector::new(cx);
    if !GetPropertyKeys(cx, object, JSITER_OWNONLY, ids.handle_mut()) {
        return Ok(Vec::new());
    }

    let mut declarations = Vec::with_capacity(ids.len());
    for id in ids.iter() {
        rooted!(&in(cx) let id = *id);

        // See if the id for the current property on the object represents a animatable CSS property.
        if !id.is_string() {
            continue;
        }
        rooted!(&in(cx) let mut key_value = UndefinedValue());
        if !JS_IdToValue(cx, id.handle().get(), key_value.handle_mut()) {
            continue;
        }
        rooted!(&in(cx) let js_string = key_value.to_string());
        let Some(js_string) = NonNull::new(js_string.get()) else {
            continue;
        };
        let property_name = unsafe { jsstr_to_string(cx, js_string) };

        let Some(property_id) = lookup_css_property_by_idl_attribute_name(&property_name) else {
            continue;
        };
        debug_assert!(property_id.is_animatable());

        // Step 6.1 Let raw value be the result of calling the [[Get]] internal method on keyframe input,
        // with property name as the property key and keyframe input as the receiver.
        // Step 6.2 Check the completion record of raw value.
        rooted!(&in(cx) let mut property_value = UndefinedValue());
        if !unsafe {
            JS_GetPropertyById(
                cx.raw_cx(),
                object.into_handle(),
                id.handle().into_handle(),
                property_value.handle_mut().into_handle_mut(),
            )
        } {
            continue;
        }

        // Step 6.3 Convert raw value to a DOMString or to a sequence of DOMStrings property values as follows:
        // If allow lists is true, [..] (Note: We don't implement "allow lists")
        // Otherwise,
        // Let property values be the result of converting raw value to a DOMString using the procedure
        // for converting an ECMAScript value to a DOMString [WEBIDL].
        let property_value = match DOMString::from_jsval(
            cx,
            property_value.handle(),
            StringificationBehavior::Default,
        ) {
            Ok(ConversionResult::Success(property_value)) => property_value,
            Ok(ConversionResult::Failure(error_message)) => {
                return Err(Error::Operation(
                    error_message
                        .to_str()
                        .ok()
                        .map(|message| message.to_owned()),
                ));
            },
            Err(_) => return Err(Error::JSFailed),
        };

        // Step 6.4 Calculate the normalized property name as the result of applying the IDL attribute name
        // to animation property name algorithm to property name.
        // Note: Due to the way our implementation differs from the spec (refer to the comment at
        // the top of this function), we already have the normalized property name.

        // Parse the property value as a value for the given animatable CSS property.
        // The specification continues to treat the value as a plain string, but there's not much point.
        let Some(declaration) =
            parse_single_property_declaration(property_id, &property_value.str(), parser_context)
        else {
            continue;
        };

        // Step 6.5 Add a property to keyframe output with normalized property name as the property name,
        // and property values as the property value.
        declarations.push(declaration);
    }

    Ok(declarations)
}

/// Parses `input` as a value for `property`, returning `None` on failure.
fn parse_single_property_declaration(
    property: NonCustomPropertyId,
    input: &str,
    parser_context: &mut ParserContext<'_>,
) -> Option<KeyframePropertyDeclaration> {
    let mut declaration = SourcePropertyDeclaration::default();
    let mut parser = Parser::new(input);

    // TODO: Consider reporting parse errors somewhere useful, like the devtools console.
    parser
        .parse_entirely(|parser| {
            PropertyDeclaration::parse_into(
                &mut declaration,
                PropertyId::NonCustom(property),
                parser_context,
                parser,
            )
        })
        .ok()?;

    let mut block = PropertyDeclarationBlock::new();
    block.extend(declaration.drain(), Importance::Normal);

    Some(KeyframePropertyDeclaration {
        property_id: PropertyId::NonCustom(property),
        block,
    })
}

/// This is the inverse of <https://drafts.csswg.org/web-animations-1/#animation-property-name-to-idl-attribute-name>.
fn lookup_css_property_by_idl_attribute_name(attribute_name: &str) -> Option<NonCustomPropertyId> {
    // TODO: Step 1. If property follows the <custom-property-name> production, return property.

    // Step 2. If property refers to the CSS float property, return the string "cssFloat".
    if attribute_name == "cssFloat" {
        return Some(LonghandId::Float.into());
    }

    // Step 3. If property refers to the CSS offset property, return the string "cssOffset".
    // NOTE: "offset" is not supported yet.

    // Step 4. Otherwise, return the result of applying the CSS property to IDL attribute algorithm
    // [CSSOM] to property.
    static IDL_ATTRIBUTE_TO_ANIMATED_PROPERTY_LOOKUP_TABLE: LazyLock<
        FxHashMap<String, NonCustomPropertyId>,
    > = LazyLock::new(|| {
        log::debug!("Initializing map from IDL attribute names to CSS properties");

        NonCustomPropertyId::iter()
            .filter(|non_custom_property| non_custom_property.is_animatable())
            .filter(|non_custom_property| {
                non_custom_property
                    .to_property_id()
                    .enabled_for_all_content()
            })
            .map(|non_custom_property| {
                let idl_attribute_name =
                    animation_property_name_to_idl_attribute_name(non_custom_property.name());
                (idl_attribute_name, non_custom_property)
            })
            .collect()
    });
    IDL_ATTRIBUTE_TO_ANIMATED_PROPERTY_LOOKUP_TABLE
        .get(attribute_name)
        .copied()
}

/// <https://drafts.csswg.org/web-animations-1/#animation-property-name-to-idl-attribute-name>
///
/// # Panics
/// Panics when the property name is empty or consists only of `-` characters.
/// In general it is assumed that `property_name` is a CSS property.
fn animation_property_name_to_idl_attribute_name(property_name: &str) -> String {
    let mut idl_attribute_name = String::with_capacity(property_name.len());

    let mut chunks = property_name.split('-');
    let Some(first_chunk) = chunks.next() else {
        unreachable!("CSS property name should not consist only of dashes");
    };
    idl_attribute_name.push_str(first_chunk);
    for chunk in chunks {
        let mut characters = chunk.chars();
        let Some(to_capitalize) = characters.next() else {
            continue;
        };
        idl_attribute_name.push(to_capitalize.to_ascii_uppercase());
        idl_attribute_name.push_str(characters.as_str());
    }

    idl_attribute_name
}
