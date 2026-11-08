// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Stillwater passwords: fill a saved login into the page's sign-in form. Stillwater replaces the
// __NAME__ placeholders with JSON values. It checks the page's origin itself, so a login is never
// filled into a page that navigated elsewhere in the meantime.

(function (origin, username, password, force) {
  "use strict";
  if (window !== window.top || location.origin !== origin) return false;
  const visible = (element) =>
    element.offsetWidth > 0 || element.offsetHeight > 0 || element.getClientRects().length > 0;
  const field = Array.from(document.querySelectorAll("input[type=password]"))
    .find((input) => visible(input) && !input.disabled && !input.readOnly);
  // Some sites ask for the username first, on its own: fill that.
  const scope = field ? (field.form || document) : document;
  const inputs = Array.from(scope.querySelectorAll("input"));
  const before = field ? inputs.slice(0, inputs.indexOf(field)).reverse() : inputs;
  const textLike = before.filter((input) =>
    ["text", "email", "tel", ""].includes(input.type) && visible(input) && !input.disabled &&
    !input.readOnly);
  const user = textLike.find((input) => /username|email/.test(input.autocomplete || "")) ||
    (field ? textLike[0] : textLike.find((input) =>
      /user|mail|login|account|identifier/i.test(input.name + " " + input.id + " " + input.autocomplete)));
  if (!field && !user) return false;
  // Don't overwrite what the user is typing unless they picked this login.
  if (!force && ((field && field.value) || (!field && user.value))) return false;
  const setValue = (input, value) => {
    Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value").set.call(input, value);
    input.dispatchEvent(new Event("input", { bubbles: true }));
    input.dispatchEvent(new Event("change", { bubbles: true }));
  };
  if (user && username && (force || !user.value)) setValue(user, username);
  if (field) setValue(field, password);
  return true;
})(__ORIGIN__, __USERNAME__, __PASSWORD__, __FORCE__)
