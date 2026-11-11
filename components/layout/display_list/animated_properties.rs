/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Animated `transform` and `opacity` values bound as WebRender dynamic properties.
//!
//! A CSS animation or transition restyles its element on every animation tick. Without this
//! module every tick rebuilds the stacking context tree and the display list of the whole page
//! and makes WebRender build a new scene: on a long page (22,000 elements) one small pulsing dot
//! costs 20+ ms of layout and as much scene building per frame, forever.
//!
//! Instead, the reference frames and opacity filters of fragments that are animating are bound
//! to WebRender property keys. When a restyle changed nothing but those properties, layout
//! computes the new values and sends them to `Paint`, which hands them to WebRender as dynamic
//! properties: the next frame shows them without a new display list or scene. Anything else
//! (other properties, box changes, a transform becoming non-invertible...) takes the normal path.

use std::sync::Arc;

use app_units::Au;
use paint_api::display_list::ScrollTree;
use rustc_hash::{FxHashMap, FxHashSet};
use style::dom::OpaqueNode;
use style::properties::ComputedValues;
use style::selector_parser::PseudoElement;
use webrender_api::units::LayoutTransform;
use webrender_api::{self as wr, PropertyBindingKey, PropertyValue};

use crate::fragment_tree::BoxFragment;
use crate::geom::PhysicalRect;

/// An element (or one of its pseudo-elements), as CSS animations key it.
pub(crate) type AnimatedKey = (OpaqueNode, Option<PseudoElement>);

/// Values for animated transform and opacity bindings.
pub(crate) type AnimatedPropertyValues =
    (Vec<PropertyValue<LayoutTransform>>, Vec<PropertyValue<f32>>);

/// A fragment whose `transform` is animating, and the reference frame it established.
pub(crate) struct AnimatedTransform {
    pub fragment: Arc<BoxFragment>,
    pub containing_block_rect: PhysicalRect<Au>,
    /// The key bound to the reference frame, or `None` if the transform was not invertible
    /// when the stacking context tree was built: then the fragment is not displayed at all.
    pub key: Option<PropertyBindingKey<LayoutTransform>>,
}

/// A fragment whose `opacity` is animating, and the key bound to its opacity filter.
pub(crate) struct AnimatedOpacity {
    pub fragment: Arc<BoxFragment>,
    pub key: PropertyBindingKey<f32>,
}

/// The animated property bindings of one stacking context tree and its display list.
#[derive(Default)]
pub(crate) struct AnimatedProperties {
    /// The elements and pseudo-elements with running animations or transitions.
    pub animating: FxHashSet<AnimatedKey>,
    /// Set while building the stacking context tree.
    pub transforms: FxHashMap<AnimatedKey, Vec<AnimatedTransform>>,
    /// The binding key of each animated reference frame, by scroll tree node index, with the
    /// fragment and containing block to compute its current transform from.
    pub reference_frames: FxHashMap<
        usize,
        (
            PropertyBindingKey<LayoutTransform>,
            Arc<BoxFragment>,
            PhysicalRect<Au>,
        ),
    >,
    /// Set while building the display list.
    pub opacities: FxHashMap<AnimatedKey, Vec<AnimatedOpacity>>,
    /// The high 32 bits of every key: a tag plus the pipeline, so that keys never collide
    /// with `Paint`'s own (small) keys or another pipeline's.
    key_namespace: u64,
    next_key: u32,
}

impl AnimatedProperties {
    pub(crate) fn new(pipeline_id: wr::PipelineId, animating: FxHashSet<AnimatedKey>) -> Self {
        Self {
            animating,
            key_namespace: (0x5357_0000 | (pipeline_id.1 as u64 & 0xffff)) << 32,
            ..Default::default()
        }
    }

    fn next_key<T>(&mut self) -> PropertyBindingKey<T> {
        self.next_key = self.next_key.wrapping_add(1);
        PropertyBindingKey::new(self.key_namespace | self.next_key as u64)
    }

    /// The animation key of `fragment` if it is animating.
    pub(crate) fn animating_key(&self, fragment: &BoxFragment) -> Option<AnimatedKey> {
        if self.animating.is_empty() {
            return None;
        }
        let tag = fragment.base.tag?;
        if tag.pseudo_element_chain.secondary.is_some() {
            return None;
        }
        let key = (tag.node, tag.pseudo_element_chain.primary);
        self.animating.contains(&key).then_some(key)
    }

    /// Register the reference frame of an animating fragment and return its binding key.
    pub(crate) fn add_transform(
        &mut self,
        key: AnimatedKey,
        fragment: &Arc<BoxFragment>,
        containing_block_rect: PhysicalRect<Au>,
        scroll_tree_node_index: Option<usize>,
    ) -> Option<PropertyBindingKey<LayoutTransform>> {
        let binding_key = scroll_tree_node_index.map(|index| {
            let binding_key = self.next_key();
            self.reference_frames.insert(
                index,
                (binding_key, fragment.clone(), containing_block_rect),
            );
            binding_key
        });
        self.transforms
            .entry(key)
            .or_default()
            .push(AnimatedTransform {
                fragment: fragment.clone(),
                containing_block_rect,
                key: binding_key,
            });
        binding_key
    }

    /// Register the opacity filter of an animating fragment and return its binding key.
    pub(crate) fn add_opacity(
        &mut self,
        key: AnimatedKey,
        fragment: &Arc<BoxFragment>,
    ) -> PropertyBindingKey<f32> {
        let binding_key = self.next_key();
        self.opacities
            .entry(key)
            .or_default()
            .push(AnimatedOpacity {
                fragment: fragment.clone(),
                key: binding_key,
            });
        binding_key
    }

    /// The binding key and current transform of the reference frame at `index` of the scroll
    /// tree, if it is animated. The transform stored in the scroll tree is the one from when the
    /// stacking context tree was built, and may be out of date.
    pub(crate) fn reference_frame(
        &self,
        index: usize,
    ) -> Option<(PropertyBindingKey<LayoutTransform>, Option<LayoutTransform>)> {
        let (key, fragment, containing_block_rect) = self.reference_frames.get(&index)?;
        let transform = fragment
            .animated_reference_frame_transform(containing_block_rect)
            .filter(LayoutTransform::is_invertible);
        Some((*key, transform))
    }

    /// Bring the transforms of the animated reference frames in `scroll_tree` up to date
    /// with their fragments' styles. Hit testing and geometry queries (`elementFromPoint`,
    /// `getBoundingClientRect`, where a click lands) read them from this tree, so it has to
    /// follow what the bindings show on screen.
    pub(crate) fn refresh_scroll_tree(&self, scroll_tree: &mut ScrollTree) {
        for (index, (_, fragment, containing_block_rect)) in &self.reference_frames {
            if let Some(transform) = fragment
                .animated_reference_frame_transform(containing_block_rect)
                .filter(LayoutTransform::is_invertible)
            {
                scroll_tree.set_reference_frame_transform(*index, transform.into());
            }
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.transforms.is_empty() && self.opacities.is_empty()
    }

    /// After a restyle that changed only the transforms of `changed_transforms` and the
    /// opacities of `changed_opacities` (fragment styles already repaired), return the current
    /// values of all bindings, or `None` if the display list must be rebuilt instead.
    pub(crate) fn current_values(
        &self,
        changed_transforms: &FxHashSet<AnimatedKey>,
        changed_opacities: &FxHashSet<AnimatedKey>,
    ) -> Option<AnimatedPropertyValues> {
        // Whether every fragment of `key` is hidden by a non-invertible transform, now too.
        let not_displayed = |key: &AnimatedKey| {
            self.transforms.get(key).is_some_and(|entries| {
                entries.iter().all(|entry| {
                    entry.key.is_none() &&
                        entry
                            .fragment
                            .animated_reference_frame_transform(&entry.containing_block_rect)
                            .is_some_and(|transform| !transform.is_invertible())
                })
            })
        };

        for key in changed_transforms {
            let entries = self.transforms.get(key)?;
            for entry in entries {
                // A transform that went away, or became (non-)invertible, changes the structure
                // of the stacking context tree.
                let transform = entry
                    .fragment
                    .animated_reference_frame_transform(&entry.containing_block_rect)?;
                if transform.is_invertible() != entry.key.is_some() {
                    return None;
                }
            }
        }
        for key in changed_opacities {
            if !self.opacities.contains_key(key) && !not_displayed(key) {
                return None;
            }
        }

        let mut transforms = Vec::new();
        for entry in self.transforms.values().flatten() {
            let Some(binding_key) = entry.key else {
                continue;
            };
            let value = entry
                .fragment
                .animated_reference_frame_transform(&entry.containing_block_rect)?;
            if !value.is_invertible() {
                return None;
            }
            transforms.push(PropertyValue {
                key: binding_key,
                value,
            });
        }
        let floats = self
            .opacities
            .values()
            .flatten()
            .map(|entry| PropertyValue {
                key: entry.key,
                value: entry.fragment.style().get_effects().opacity,
            })
            .collect();
        Some((transforms, floats))
    }
}

/// Compare two styles of the same element. Returns `None` if they differ in anything but
/// `transform` (and its individual properties and origin) and `opacity`, otherwise whether the
/// transform and whether the opacity changed.
pub(crate) fn compositable_difference(
    old: &ComputedValues,
    new: &ComputedValues,
) -> Option<(bool, bool)> {
    if std::ptr::eq(old, new) {
        return Some((false, false));
    }
    macro_rules! same {
        ($($getter:ident),*) => {
            $(
                if old.$getter() != new.$getter() {
                    return None;
                }
            )*
        };
    }
    same!(
        get_background,
        get_border,
        get_column,
        get_counters,
        get_font,
        get_inherited_box,
        get_inherited_svg,
        get_inherited_table,
        get_inherited_text,
        get_inherited_ui,
        get_list,
        get_margin,
        get_outline,
        get_padding,
        get_position,
        get_svg,
        get_table,
        get_text,
        get_ui
    );
    if old.effective_zoom != new.effective_zoom {
        return None;
    }

    let (old_box, new_box) = (old.get_box(), new.get_box());
    let transform_changed = old_box.transform != new_box.transform ||
        old_box.rotate != new_box.rotate ||
        old_box.scale != new_box.scale ||
        old_box.translate != new_box.translate ||
        old_box.transform_origin != new_box.transform_origin;
    if transform_changed {
        let mut patched = old_box.clone();
        patched.transform = new_box.transform.clone();
        patched.rotate = new_box.rotate.clone();
        patched.scale = new_box.scale.clone();
        patched.translate = new_box.translate.clone();
        patched.transform_origin = new_box.transform_origin.clone();
        if patched != *new_box {
            return None;
        }
    } else if old_box != new_box {
        return None;
    }

    let (old_effects, new_effects) = (old.get_effects(), new.get_effects());
    let opacity_changed = old_effects.opacity != new_effects.opacity;
    if opacity_changed {
        let mut patched = old_effects.clone();
        patched.opacity = new_effects.opacity;
        if patched != *new_effects {
            return None;
        }
    } else if old_effects != new_effects {
        return None;
    }

    Some((transform_changed, opacity_changed))
}
