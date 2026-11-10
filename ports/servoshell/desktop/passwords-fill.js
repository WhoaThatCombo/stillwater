// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Stillwater passwords: fill one saved login into the page's sign-in form. Stillwater replaces
// the arguments placeholder at the end with a JSON array (last, so nothing in a username or
// password is ever taken for a placeholder) and runs this in the top-level page only. It checks
// the page's origin itself (`location` and `window.top` can't be replaced by the page), so a
// login is never filled into a page that navigated elsewhere in the meantime.
//
// Modes:
//   "load":    the page just showed a sign-in form; fill only empty fields, and not a password
//              when the username field holds a different username.
//   "continue": the next step of a sign-in where the user picked this login: fill only an
//              empty password field (no username field, or one with this username).
//   "focused": the user picked this login from the list under the focused field: fill that
//              field's form.
//   "page":    the user picked this login in the sidebar: fill the page's first sign-in form.

(function (origin, username, password, mode) {
  "use strict";
  if (window !== window.top || location.origin !== origin) return false;

__DETECT__

  let group = null;
  if (mode === "focused") {
    const field = focusedElement();
    group = field instanceof HTMLInputElement ? groupOf(field) : null;
  } else {
    group = loginGroups()[0] || null;
  }
  if (!group) return false;
  const { user, password: field } = group;
  // "continue": the password step of a sign-in that started with a login picked from the list;
  // only a password field, with no username field or one holding that login's username.
  if (mode === "continue" &&
      (!field || field.value || (user && user.value.trim() !== username))) return false;
  const force = mode !== "load" && mode !== "continue";
  if (!force) {
    if (field && field.value) return false;
    // The user typed another username: leave the password to them.
    if (user && user.value && user.value.trim() !== username) return false;
  }
  // Set the value the way typing would, so pages built with React, Vue and the like notice.
  const setter = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value").set;
  const setValue = (input, value) => {
    setter.call(input, value);
    input.dispatchEvent(new Event("input", { bubbles: true, composed: true }));
    input.dispatchEvent(new Event("change", { bubbles: true }));
  };
  if (user && username && (force || !user.value)) setValue(user, username);
  if (field) setValue(field, password);
  return true;
})(...__ARGUMENTS__)
