/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

// https://html.spec.whatwg.org/multipage/#htmlallcollection
// HTMLAllCollection objects have an [[IsHTMLDDA]] internal slot: the reflector's
// class is created with JSCLASS_EMULATES_UNDEFINED (see the binding codegen).
// The custom [[Call]] behaviour (legacycaller) is not supported.
[Exposed=Window, LegacyUnenumerableNamedProperties]
interface HTMLAllCollection {
  readonly attribute unsigned long length;
  getter Element? (unsigned long index);
  getter (HTMLCollection or Element)? namedItem(DOMString name);
  (HTMLCollection or Element)? item(optional DOMString nameOrIndex);
};
