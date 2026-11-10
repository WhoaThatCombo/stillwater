/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::Cell;

use atomic_refcell::AtomicRefCell;
use base64::Engine as _;
use cssparser::Parser;
use dom_struct::dom_struct;
use euclid::default::Transform3D;
use html5ever::{LocalName, Prefix, local_name, ns};
use js::context::{JSContext, NoGC};
use js::rust::HandleObject;
use layout_api::SVGElementData;
use servo_base::cross_process_instant::CrossProcessInstant;
use servo_url::ServoUrl;
use style::attr::AttrValue;
use style::parser::ParserContext;
use style::properties::{ComputedValues, LonghandId, PropertyDeclarationId};
use style::stylesheets::Origin;
use style::values::specified::LengthPercentage;
use style_traits::ParsingMode;
use stylo_atoms::Atom;
use uuid::Uuid;
use xml5ever::serialize::TraversalScope;

use crate::dom::bindings::codegen::Bindings::DocumentBinding::DocumentMethods;
use crate::dom::bindings::codegen::Bindings::NodeBinding::NodeMethods;
use crate::dom::bindings::codegen::Bindings::SVGSVGElementBinding::SVGSVGElementMethods;
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::num::Finite;
use crate::dom::bindings::root::{DomRoot, LayoutDom};
use crate::dom::bindings::str::DOMString;
use crate::dom::document::Document;
use crate::dom::element::attributes::storage::AttrRef;
use crate::dom::element::{AttributeMutation, Element};
use crate::dom::geometry::dommatrix::DOMMatrix;
use crate::dom::geometry::dompoint::DOMPoint;
use crate::dom::geometry::domrect::DOMRect;
use crate::dom::iterators::ShadowIncluding;
use crate::dom::node::virtualmethods::VirtualMethods;
use crate::dom::node::{
    ChildrenMutation, CloneChildrenFlag, Node, NodeDamage, NodeTraits, UnbindContext,
};
use crate::dom::svg::svggraphicselement::SVGGraphicsElement;

#[dom_struct]
pub(crate) struct SVGSVGElement {
    svggraphicselement: SVGGraphicsElement,
    #[no_trace]
    uuid: Uuid,
    // The XML source of subtree rooted at this SVG element, serialized into
    // a base64 encoded `data:` url. This is cached to avoid recomputation
    // on each layout and must be invalidated when the subtree changes.
    #[no_trace]
    cached_serialized_data_url: AtomicRefCell<Option<Result<ServoUrl, ()>>>,
    /// The document time container of SVG 1.1 (getCurrentTime() and friends).
    /// <https://www.w3.org/TR/SVG11/struct.html#InterfaceSVGSVGElement>
    time_container: TimeContainer,
    /// Stillwater: the computed styles the serialized copy was made with. See
    /// [`SVGSVGElement::invalidate_if_styles_changed`].
    #[no_trace]
    style_snapshot: AtomicRefCell<StyleSnapshot>,
    /// Stillwater: whether this element is in [`Window`]'s list of serialized `<svg>`s.
    registered: Cell<bool>,
    /// Stillwater: whether this `<svg>`'s copy includes elements of another `<svg>` through
    /// `<use>` (a sprite sheet), so it has to be made again when that other `<svg>` changes.
    uses_other_svgs: Cell<bool>,
    /// Stillwater: whether other `<svg>`s include elements of this one through `<use>`.
    used_by_other_svgs: Cell<bool>,
}

/// Stillwater: the serialized copy of an inline `<svg>` is rendered on its own, without the
/// page's style sheets, so the values of these properties (which page CSS often sets, as in
/// `.icon { fill: currentColor }` or `button:hover svg { fill: blue }`) are written into the
/// copy's `style` attributes.
///
/// `color` itself is not among them: `currentColor` is resolved when baking, so that animating
/// `color` on an ancestor only redraws the icons that actually use it.
const INLINED_PROPERTIES: [(LonghandId, &str); 13] = [
    (LonghandId::Fill, "fill"),
    (LonghandId::FillOpacity, "fill-opacity"),
    (LonghandId::FillRule, "fill-rule"),
    (LonghandId::Stroke, "stroke"),
    (LonghandId::StrokeWidth, "stroke-width"),
    (LonghandId::StrokeOpacity, "stroke-opacity"),
    (LonghandId::StrokeLinecap, "stroke-linecap"),
    (LonghandId::StrokeLinejoin, "stroke-linejoin"),
    (LonghandId::StrokeDasharray, "stroke-dasharray"),
    (LonghandId::StrokeDashoffset, "stroke-dashoffset"),
    (LonghandId::StrokeMiterlimit, "stroke-miterlimit"),
    (LonghandId::Opacity, "opacity"),
    (LonghandId::Visibility, "visibility"),
];

/// The computed styles of the elements of an `<svg>` subtree, in tree order.
#[derive(Default, MallocSizeOf)]
struct StyleSnapshot {
    /// Each element's computed values; these are replaced whenever it is restyled.
    #[ignore_malloc_size_of = "shared with the elements"]
    styles: Vec<Option<servo_arc::Arc<ComputedValues>>>,
    /// The declarations written into each element's copy, if it has a style.
    declarations: Vec<Option<String>>,
}

impl SVGSVGElement {
    fn new_inherited(
        local_name: LocalName,
        prefix: Option<Prefix>,
        document: &Document,
    ) -> SVGSVGElement {
        SVGSVGElement {
            svggraphicselement: SVGGraphicsElement::new_inherited(local_name, prefix, document),
            uuid: Uuid::new_v4(),
            cached_serialized_data_url: Default::default(),
            time_container: Default::default(),
            style_snapshot: Default::default(),
            registered: Default::default(),
            uses_other_svgs: Default::default(),
            used_by_other_svgs: Default::default(),
        }
    }

    #[cfg_attr(crown, allow(crown::unrooted_must_root))]
    pub(crate) fn new(
        cx: &mut js::context::JSContext,
        local_name: LocalName,
        prefix: Option<Prefix>,
        document: &Document,
        proto: Option<HandleObject>,
    ) -> DomRoot<SVGSVGElement> {
        Node::reflect_node_with_proto(
            cx,
            Box::new(SVGSVGElement::new_inherited(local_name, prefix, document)),
            document,
            proto,
        )
    }

    pub(crate) fn serialize_and_cache_subtree(&self, cx: &mut js::context::JSContext) {
        let document_fragment = self.owner_document().CreateDocumentFragment(cx);
        let cloned_node = Node::clone(
            cx,
            self.upcast(),
            None,
            CloneChildrenFlag::CloneChildren,
            None,
        );
        if document_fragment
            .upcast::<Node>()
            .AppendChild(cx, &cloned_node)
            .is_err()
        {
            error!("Unable to clone SVG tree");
            *self.cached_serialized_data_url.borrow_mut() = Some(Err(()));
            return;
        }

        // Bake the computed styles into the copy, in tree order like the original.
        let snapshot = self.take_style_snapshot(true);
        let clones: Vec<_> = cloned_node
            .traverse_preorder(ShadowIncluding::No)
            .filter_map(DomRoot::downcast::<Element>)
            .collect();
        for (clone, declarations) in clones.iter().zip(&snapshot.declarations) {
            if let Some(declarations) = declarations {
                let existing = clone.get_string_attribute(&local_name!("style"));
                let style = if existing.is_empty() {
                    declarations.clone()
                } else {
                    format!("{existing};{declarations}")
                };
                clone.set_string_attribute(cx, &local_name!("style"), style.into());
            }
        }
        *self.style_snapshot.borrow_mut() = snapshot;
        self.owner_window().register_serialized_svg(self);

        self.uses_other_svgs.set(false);
        self.process_use_elements(cx, &cloned_node);

        let Ok(xml_source) = cloned_node.xml_serialize(TraversalScope::IncludeNode) else {
            *self.cached_serialized_data_url.borrow_mut() = Some(Err(()));
            return;
        };

        let xml_source: String = xml_source.into();
        let base64_encoded_source = base64::engine::general_purpose::STANDARD.encode(xml_source);
        let data_url = format!("data:image/svg+xml;base64,{base64_encoded_source}");
        match ServoUrl::parse(&data_url) {
            Ok(url) => {
                // Stillwater: icons with the same markup share this image; it is evicted when
                // the last of them stops using it.
                self.owner_window().retain_svg_image(&url);
                *self.cached_serialized_data_url.borrow_mut() = Some(Ok(url));
            },
            Err(error) => error!("Unable to parse serialized SVG data url: {error}"),
        };
    }

    fn subtree_elements(&self) -> impl Iterator<Item = DomRoot<Element>> + use<> {
        self.upcast::<Node>()
            .traverse_preorder(ShadowIncluding::No)
            .filter_map(DomRoot::downcast::<Element>)
    }

    /// The computed styles of this subtree, with the declarations to bake in if `declarations`.
    fn take_style_snapshot(&self, declarations: bool) -> StyleSnapshot {
        let mut snapshot = StyleSnapshot::default();
        for element in self.subtree_elements() {
            // The `<svg>` element's own opacity is applied by the page around the picture.
            let is_root = *element.upcast::<Node>() == *self.upcast::<Node>();
            let style = (*element.namespace() == ns!(svg))
                .then(|| element.style_without_reflow())
                .flatten();
            snapshot.styles.push(style.clone());
            if declarations {
                snapshot.declarations.push(style.map(|style| {
                    let mut css = String::new();
                    let color = style.computed_value_to_string(PropertyDeclarationId::Longhand(
                        LonghandId::Color,
                    ));
                    for (id, name) in INLINED_PROPERTIES {
                        let mut value =
                            style.computed_value_to_string(PropertyDeclarationId::Longhand(id));
                        if value.eq_ignore_ascii_case("currentcolor") {
                            value = color.clone();
                        }
                        // Paint servers (`url(#gradient)`) stay as the markup has them.
                        if !value.starts_with("url(") && !(is_root && id == LonghandId::Opacity) {
                            css.push_str(&format!("{name}:{value};"));
                        }
                    }
                    css
                }));
            }
        }
        snapshot
    }

    /// Whether no element of this subtree was restyled since the copy was made. This runs
    /// for many `<svg>`s after each layout, so it neither roots nodes nor allocates.
    fn styles_unchanged(&self, no_gc: &NoGC) -> bool {
        let snapshot = self.style_snapshot.borrow();
        let mut baked = snapshot.styles.iter();
        for node in self
            .upcast::<Node>()
            .traverse_preorder_unrooted(no_gc, ShadowIncluding::No)
        {
            let Some(element) = node.downcast::<Element>() else {
                continue;
            };
            let Some(baked) = baked.next() else {
                return false;
            };
            let same = if *element.namespace() == ns!(svg) {
                element.primary_style_is(baked.as_ref())
            } else {
                baked.is_none()
            };
            if !same {
                return false;
            }
        }
        baked.next().is_none()
    }

    /// Stillwater: called after each layout. A style change (a class toggled on an ancestor,
    /// `:hover`) does not touch the subtree, so it has to be noticed here. Restyled elements get
    /// new computed values, and only when those differ in the baked-in properties does the
    /// copy have to be made, and rasterized, again.
    pub(crate) fn invalidate_if_styles_changed(&self, no_gc: &NoGC) {
        if !matches!(&*self.cached_serialized_data_url.borrow(), Some(Ok(_))) {
            return;
        }
        if self.styles_unchanged(no_gc) {
            return;
        }
        let current = self.take_style_snapshot(true);
        let mut snapshot = self.style_snapshot.borrow_mut();
        // Values on elements with running CSS animations or transitions are skipped: following
        // them would mean rasterizing the picture again every frame. The final values are
        // compared once they stop.
        let document = self.owner_document();
        let animations = document.animation_manager();
        let changed = current.declarations.len() != snapshot.declarations.len() ||
            self.subtree_elements()
                .zip(current.declarations.iter().zip(&snapshot.declarations))
                .any(|(element, (current, baked))| {
                    current != baked && !animations.node_is_animating(element.upcast())
                });
        if !changed {
            snapshot.styles = current.styles;
            return;
        }
        drop(snapshot);
        self.invalidate_cached_serialized_subtree_and_rasterization_result(no_gc);
    }

    fn process_use_elements(&self, cx: &mut JSContext, root_node: &Node) {
        for node in root_node.traverse_preorder(ShadowIncluding::No) {
            if let Some(element) = node.downcast::<Element>() &&
                element.local_name() == &local_name!("use")
            {
                self.process_single_use_element(cx, element, root_node)
            }
        }
    }

    fn process_single_use_element(
        &self,
        cx: &mut JSContext,
        use_element: &Element,
        root_node: &Node,
    ) {
        let href = use_element
            .get_attribute_string_value(&local_name!("href"))
            .or_else(|| {
                use_element
                    .get_attribute_string_value_with_namespace(&ns!(xlink), &local_name!("href"))
            })
            .unwrap_or_default();
        let Some(id_string) = href.strip_prefix("#").map(DOMString::from) else {
            return;
        };

        let document = self.upcast::<Node>().owner_doc();
        let Some(referenced_element) = document.GetElementById(cx, id_string) else {
            return;
        };
        let referenced_node = referenced_element.upcast::<Node>();

        // Don't use this node if it doesn't have an `<svg>` ancestor.
        if !referenced_node
            .inclusive_ancestors_unrooted(cx.no_gc(), ShadowIncluding::No)
            .any(|ancestor| ancestor.is::<SVGSVGElement>())
        {
            return;
        };

        // Don't use this node if it already exists within the same `<svg>` element.
        if referenced_node
            .inclusive_ancestors_unrooted(cx.no_gc(), ShadowIncluding::No)
            .any(|ancestor| *ancestor == self.upcast())
        {
            return;
        };

        // Stillwater: remember the dependency, so that a change in the sprite sheet reaches
        // this `<svg>` too.
        self.uses_other_svgs.set(true);
        for ancestor in referenced_node.inclusive_ancestors(ShadowIncluding::No) {
            if let Some(svg) = ancestor.downcast::<SVGSVGElement>() {
                svg.used_by_other_svgs.set(true);
            }
        }

        let cloned_node = Node::clone(
            cx,
            referenced_node,
            None,
            CloneChildrenFlag::CloneChildren,
            None,
        );
        let _ = root_node.AppendChild(cx, &cloned_node);
    }

    pub(crate) fn is_registered(&self) -> bool {
        self.registered.get()
    }

    pub(crate) fn set_registered(&self, registered: bool) {
        self.registered.set(registered);
    }

    pub(crate) fn uses_other_svgs(&self) -> bool {
        self.uses_other_svgs.get()
    }

    pub(crate) fn invalidate_cached_serialized_subtree_and_rasterization_result(
        &self,
        no_gc: &NoGC,
    ) {
        let owner_window = self.owner_window();
        // The picture on screen stays until the new one is ready (see
        // `ImageCache::last_rasterized_svg`), so a change never blinks.
        owner_window
            .image_cache()
            .evict_rasterized_image(&self.uuid, true);
        if self.used_by_other_svgs.get() {
            owner_window.invalidate_svgs_using_other_svgs(no_gc);
        }
        if let Some(Ok(url)) = &*self.cached_serialized_data_url.borrow() &&
            !owner_window.release_svg_image(url)
        {
            owner_window.layout_mut().remove_cached_image(url);
            owner_window.image_cache().evict_completed_image(
                url,
                owner_window.origin().immutable(),
                &None,
            );
        }

        *self.cached_serialized_data_url.borrow_mut() = None;
        self.upcast::<Node>().dirty(no_gc, NodeDamage::Other);
    }
}

/// The SVG 1.1 document time container: the clock that SMIL animations in an `<svg>`
/// fragment run on. Stillwater renders SVG without SMIL animations, but scripts still
/// pause, seek and read this clock, so it is kept faithfully.
#[derive(Default, JSTraceable, MallocSizeOf)]
struct TimeContainer {
    /// Seconds subtracted from the document's clock to get this container's time.
    offset: Cell<f64>,
    /// The document clock reading when the container was paused, if it is paused.
    paused_at: Cell<Option<f64>>,
}

impl SVGSVGElement {
    /// Seconds since the document's time origin.
    fn document_clock(&self) -> f64 {
        let window = self.owner_window();
        (CrossProcessInstant::now() - window.navigation_start()).as_seconds_f64()
    }

    fn current_time(&self) -> f64 {
        let clock = self
            .time_container
            .paused_at
            .get()
            .unwrap_or_else(|| self.document_clock());
        (clock - self.time_container.offset.get()).max(0.0)
    }
}

impl SVGSVGElementMethods<crate::DomTypeHolder> for SVGSVGElement {
    /// <https://svgwg.org/svg2-draft/struct.html#__svg__SVGSVGElement__createSVGPoint>
    fn CreateSVGPoint(&self, cx: &mut JSContext) -> DomRoot<DOMPoint> {
        DOMPoint::new(cx, &self.owner_global(), 0.0, 0.0, 0.0, 1.0)
    }

    /// <https://svgwg.org/svg2-draft/struct.html#__svg__SVGSVGElement__createSVGMatrix>
    fn CreateSVGMatrix(&self, cx: &mut JSContext) -> DomRoot<DOMMatrix> {
        DOMMatrix::new(cx, &self.owner_global(), true, Transform3D::identity())
    }

    /// <https://svgwg.org/svg2-draft/struct.html#__svg__SVGSVGElement__createSVGRect>
    fn CreateSVGRect(&self, cx: &mut JSContext) -> DomRoot<DOMRect> {
        DOMRect::new(cx, &self.owner_global(), 0.0, 0.0, 0.0, 0.0)
    }

    /// <https://svgwg.org/svg2-draft/struct.html#__svg__SVGSVGElement__getElementById>
    fn GetElementById(&self, element_id: DOMString) -> Option<DomRoot<Element>> {
        // > Searches this SVG document fragment (i.e., the search is restricted to a
        // > subset of the document tree) for an Element whose id property matches elementId.
        let id = Atom::from(element_id);
        self.upcast::<Node>()
            .traverse_preorder(ShadowIncluding::No)
            .skip(1)
            .filter_map(DomRoot::downcast::<Element>)
            .find(|element| element.get_id().as_ref() == Some(&id))
    }

    /// <https://svgwg.org/svg2-draft/struct.html#__svg__SVGSVGElement__suspendRedraw>
    fn SuspendRedraw(&self, _max_wait_milliseconds: u32) -> u32 {
        // > ... has no effect and returns 1.
        1
    }

    /// <https://svgwg.org/svg2-draft/struct.html#__svg__SVGSVGElement__unsuspendRedraw>
    fn UnsuspendRedraw(&self, _suspend_handle_id: u32) {}

    /// <https://svgwg.org/svg2-draft/struct.html#__svg__SVGSVGElement__unsuspendRedrawAll>
    fn UnsuspendRedrawAll(&self) {}

    /// <https://svgwg.org/svg2-draft/struct.html#__svg__SVGSVGElement__forceRedraw>
    fn ForceRedraw(&self) {}

    /// <https://www.w3.org/TR/SVG11/struct.html#__svg__SVGSVGElement__pauseAnimations>
    fn PauseAnimations(&self) {
        if self.time_container.paused_at.get().is_none() {
            self.time_container
                .paused_at
                .set(Some(self.document_clock()));
        }
    }

    /// <https://www.w3.org/TR/SVG11/struct.html#__svg__SVGSVGElement__unpauseAnimations>
    fn UnpauseAnimations(&self) {
        if let Some(paused_at) = self.time_container.paused_at.take() {
            let paused_for = self.document_clock() - paused_at;
            self.time_container
                .offset
                .set(self.time_container.offset.get() + paused_for);
        }
    }

    /// <https://www.w3.org/TR/SVG11/struct.html#__svg__SVGSVGElement__animationsPaused>
    fn AnimationsPaused(&self) -> bool {
        self.time_container.paused_at.get().is_some()
    }

    /// <https://www.w3.org/TR/SVG11/struct.html#__svg__SVGSVGElement__getCurrentTime>
    fn GetCurrentTime(&self) -> Finite<f32> {
        Finite::wrap(self.current_time() as f32)
    }

    /// <https://www.w3.org/TR/SVG11/struct.html#__svg__SVGSVGElement__setCurrentTime>
    fn SetCurrentTime(&self, seconds: Finite<f32>) {
        let seconds = (*seconds as f64).max(0.0);
        let clock = self
            .time_container
            .paused_at
            .get()
            .unwrap_or_else(|| self.document_clock());
        self.time_container.offset.set(clock - seconds);
    }
}
impl<'dom> LayoutDom<'dom, SVGSVGElement> {
    pub(crate) fn data(self) -> SVGElementData<'dom> {
        let svg_id = self.unsafe_get().uuid;
        let element = self.upcast::<Element>();
        let width = element.get_attr_for_layout(&ns!(), &local_name!("width"));
        let height = element.get_attr_for_layout(&ns!(), &local_name!("height"));
        let view_box = element.get_attr_for_layout(&ns!(), &local_name!("viewBox"));
        SVGElementData {
            source: self
                .unsafe_get()
                .cached_serialized_data_url
                .borrow()
                .clone(),
            width,
            height,
            view_box,
            svg_id,
        }
    }
}

impl VirtualMethods for SVGSVGElement {
    fn super_type(&self) -> Option<&dyn VirtualMethods> {
        Some(self.upcast::<SVGGraphicsElement>() as &dyn VirtualMethods)
    }

    fn attribute_mutated(
        &self,
        cx: &mut js::context::JSContext,
        attr: AttrRef<'_>,
        mutation: AttributeMutation,
    ) {
        self.super_type()
            .unwrap()
            .attribute_mutated(cx, attr, mutation);

        self.invalidate_cached_serialized_subtree_and_rasterization_result(cx.no_gc());
    }

    fn attribute_affects_presentational_hints(&self, attr: AttrRef<'_>) -> bool {
        match attr.local_name() {
            &local_name!("width") | &local_name!("height") => true,
            _ => self
                .super_type()
                .unwrap()
                .attribute_affects_presentational_hints(attr),
        }
    }

    fn parse_plain_attribute(&self, name: &LocalName, value: DOMString) -> AttrValue {
        match *name {
            local_name!("width") | local_name!("height") => {
                let value = &value.str();
                let parser = &mut Parser::new(value);
                let doc = self.owner_document();
                let url = doc.url().into_url().into();
                let context = ParserContext::new(
                    Origin::Author,
                    &url,
                    None,
                    ParsingMode::ALLOW_UNITLESS_LENGTH,
                    doc.quirks_mode(),
                    /* namespaces = */ Default::default(),
                    None,
                    None,
                    /* attr_taint = */ Default::default(),
                );
                let val = LengthPercentage::parse_quirky(
                    &context,
                    parser,
                    style::values::specified::AllowQuirks::Always,
                );
                AttrValue::LengthPercentage(value.to_string(), val.ok())
            },
            _ => self
                .super_type()
                .unwrap()
                .parse_plain_attribute(name, value),
        }
    }

    fn children_changed(&self, cx: &mut JSContext, mutation: &ChildrenMutation) {
        if let Some(super_type) = self.super_type() {
            super_type.children_changed(cx, mutation);
        }

        self.invalidate_cached_serialized_subtree_and_rasterization_result(cx.no_gc());
    }

    fn unbind_from_tree(&self, cx: &mut js::context::JSContext, context: &UnbindContext<'_>) {
        if let Some(s) = self.super_type() {
            s.unbind_from_tree(cx, context);
        }

        self.registered.set(false);
        self.owner_window()
            .image_cache()
            .evict_rasterized_image(&self.uuid, false);

        self.invalidate_cached_serialized_subtree_and_rasterization_result(cx.no_gc());
    }
}
