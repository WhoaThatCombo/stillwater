/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use html5ever::{local_name, ns};
use js::context::{JSContext, NoGC};
use script_bindings::domstring::TracedDOMString;
use script_bindings::reflector::{Reflector, reflect_dom_object};
use stylo_atoms::Atom;

use crate::dom::bindings::codegen::Bindings::HTMLAllCollectionBinding::HTMLAllCollectionMethods;
use crate::dom::bindings::codegen::Bindings::HTMLCollectionBinding::HTMLCollectionMethods;
use crate::dom::bindings::codegen::UnionTypes::HTMLCollectionOrElement;
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::{Dom, DomRoot};
use crate::dom::bindings::str::DOMString;
use crate::dom::document::Document;
use crate::dom::element::Element;
use crate::dom::html::htmlcollection::{CollectionFilter, HTMLCollection};
use crate::dom::node::Node;
use crate::dom::window::Window;

/// <https://html.spec.whatwg.org/multipage/#htmlallcollection>
///
/// The reflector of this interface has the [[IsHTMLDDA]] internal slot (its proxy class is
/// created with `JSCLASS_EMULATES_UNDEFINED`), so `document.all` is falsy, `typeof` gives
/// "undefined" and it is loosely equal to `null` and `undefined`, while still being an object
/// that is strictly different from `undefined`.
#[dom_struct]
pub(crate) struct HTMLAllCollection {
    reflector_: Reflector,
    /// All the elements in the document, in tree order.
    collection: Dom<HTMLCollection>,
}

/// <https://html.spec.whatwg.org/multipage/#all-named-elements>
fn is_all_named_element(element: &Element) -> bool {
    element.namespace() == &ns!(html) &&
        matches!(
            *element.local_name(),
            local_name!("a") |
                local_name!("button") |
                local_name!("embed") |
                local_name!("form") |
                local_name!("frame") |
                local_name!("frameset") |
                local_name!("iframe") |
                local_name!("img") |
                local_name!("input") |
                local_name!("map") |
                local_name!("meta") |
                local_name!("object") |
                local_name!("select") |
                local_name!("textarea")
        )
}

fn matches_all_name(element: &Element, name: &Atom) -> bool {
    element.get_id().is_some_and(|id| id == *name) ||
        (is_all_named_element(element) && element.get_name().is_some_and(|n| n == *name))
}

impl HTMLAllCollection {
    fn new_inherited(collection: &HTMLCollection) -> HTMLAllCollection {
        HTMLAllCollection {
            reflector_: Reflector::new(),
            collection: Dom::from_ref(collection),
        }
    }

    pub(crate) fn new(
        cx: &mut JSContext,
        window: &Window,
        document: &Document,
    ) -> DomRoot<HTMLAllCollection> {
        let collection =
            HTMLCollection::new_with_filter_fn(cx, window, document.upcast(), |_, _| true);
        reflect_dom_object(
            cx,
            Box::new(HTMLAllCollection::new_inherited(&collection)),
            window,
        )
    }

    /// <https://html.spec.whatwg.org/multipage/#concept-get-all-named>
    fn get_all_named(&self, cx: &mut JSContext, name: DOMString) -> Option<HTMLCollectionOrElement> {
        // Step 1. If name is the empty string, return null.
        if name.is_empty() {
            return None;
        }
        let name = Atom::from(name);

        // Step 2-4. A single match is returned as is, several as a collection.
        {
            let mut matches = self
                .collection
                .elements_iter(cx.no_gc())
                .filter(|element| matches_all_name(element, &name));
            let first = matches.next()?;
            if matches.next().is_none() {
                return Some(HTMLCollectionOrElement::Element(first.as_rooted()));
            }
        }

        #[derive(JSTraceable, MallocSizeOf)]
        struct AllNamedFilter {
            #[no_trace]
            name: Atom,
        }
        impl CollectionFilter for AllNamedFilter {
            fn filter(&self, element: &Element, _root: &Node) -> bool {
                matches_all_name(element, &self.name)
            }
        }
        let root = self.collection.root_node();
        let global = self.global();
        Some(HTMLCollectionOrElement::HTMLCollection(
            HTMLCollection::create(cx, global.as_window(), &root, Box::new(AllNamedFilter { name })),
        ))
    }
}

impl HTMLAllCollectionMethods<crate::DomTypeHolder> for HTMLAllCollection {
    /// <https://html.spec.whatwg.org/multipage/#dom-htmlallcollection-length>
    fn Length(&self, cx: &JSContext) -> u32 {
        self.collection.Length(cx)
    }

    /// <https://html.spec.whatwg.org/multipage/#dom-htmlallcollection-item>
    fn IndexedGetter(&self, cx: &JSContext, index: u32) -> Option<DomRoot<Element>> {
        self.collection.Item(cx, index)
    }

    /// <https://html.spec.whatwg.org/multipage/#dom-htmlallcollection-nameditem>
    fn NamedItem(&self, cx: &mut JSContext, name: DOMString) -> Option<HTMLCollectionOrElement> {
        self.get_all_named(cx, name)
    }

    /// <https://html.spec.whatwg.org/multipage/#dom-htmlallcollection-nameditem>
    fn NamedGetter(&self, cx: &mut JSContext, name: DOMString) -> Option<HTMLCollectionOrElement> {
        self.get_all_named(cx, name)
    }

    /// <https://html.spec.whatwg.org/multipage/#dom-htmlallcollection-item>
    fn Item(
        &self,
        cx: &mut JSContext,
        name_or_index: Option<DOMString>,
    ) -> Option<HTMLCollectionOrElement> {
        // Step 1. If nameOrIndex was not provided, return null.
        let name_or_index = name_or_index?;

        // <https://html.spec.whatwg.org/multipage/#concept-get-all-indexed-or-named>
        // Step 1. If nameOrIndex is an array index, return the "all"-indexed element.
        let string = name_or_index.to_string();
        if let Ok(index) = string.parse::<u32>() {
            if index != u32::MAX && index.to_string() == string {
                return self
                    .collection
                    .Item(cx, index)
                    .map(HTMLCollectionOrElement::Element);
            }
        }

        // Step 2. Return the "all"-named element(s).
        self.get_all_named(cx, name_or_index)
    }

    /// <https://html.spec.whatwg.org/multipage/#the-htmlallcollection-interface:supported-property-names>
    fn SupportedPropertyNames(&self, no_gc: &NoGC) -> Vec<DOMString> {
        let mut result: Vec<DOMString> = vec![];
        for element in self.collection.elements_iter(no_gc) {
            if let Some(id) = element.get_id() {
                let id = TracedDOMString::from(id).root();
                if !result.contains(&id) {
                    result.push(id);
                }
            }
            if is_all_named_element(&element) &&
                let Some(name) = element.get_name()
            {
                let name = TracedDOMString::from(name).root();
                if !result.contains(&name) {
                    result.push(name);
                }
            }
        }
        result
    }
}
