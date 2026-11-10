// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Stillwater passwords: tell Stillwater when the page shows a sign-in form (so it can fill a
// saved password), when a sign-in field is focused (so it can show the saved logins under it)
// and which username and password were signed in with (so it can offer to save them).
// Stillwater fills forms itself (passwords-fill.js). Messages go through a tagged console
// message that Stillwater keeps out of its logs. They never carry saved logins: the list of
// saved logins is shown by Stillwater, outside the page, and only the one picked is filled in.
// Runs in top-level pages only: logins are never filled into frames.

(function () {
  "use strict";
  if (window !== window.top) return;
  // Keep our own references: pages sometimes replace these.
  const log = console.log.bind(console);
  const stringify = JSON.stringify;
  const PREFIX = "__PREFIX__";

__DETECT__

  // ---- Offering to save what was signed in with ----

  // The username that goes with a submitted password: the marked or closest filled-in
  // text-like field before it.
  function submittedUsername(password) {
    const scope = password.form || document;
    const inputs = Array.from(scope.querySelectorAll("input"));
    const before = inputs.slice(0, inputs.indexOf(password)).reverse();
    const candidates = before.filter((input) =>
      USERNAME_TYPES.includes(input.type) && !input.disabled && input.value.trim());
    const marked = candidates.find((input) => /username|email/.test(attribute(input, "autocomplete")));
    return marked || candidates[0] || null;
  }

  let lastSent = "";
  function capture(scope) {
    const fields = Array.from((scope || document).querySelectorAll("input[type=password]"))
      .filter((input) => input.value);
    if (fields.length === 0) return;
    // Sign-up and change-password forms have two or three password fields: the new password
    // is the last distinct one (sign-up repeats it; change-password has current, new, new).
    let password = fields[0];
    if (fields.length >= 2) {
      const values = fields.map((input) => input.value);
      password = fields.length === 3 || values[0] !== values[values.length - 1]
        ? fields[fields.length - 1] : fields[0];
    }
    const user = submittedUsername(password);
    const username = user ? user.value.trim() : "";
    const payload = stringify({ u: username, p: password.value });
    if (payload === lastSent) return;
    lastSent = payload;
    log(PREFIX + "submit\t" + payload);
  }

  document.addEventListener("submit", (event) => capture(event.target), true);
  // Many sign-in pages never submit a form: they send the password from a click handler.
  document.addEventListener("click", (event) => {
    const button = event.target instanceof Element &&
      event.target.closest("button, input[type=submit], input[type=button], [role=button]");
    if (!button) return;
    const form = button.form || button.closest("form");
    capture(form || document);
  }, true);
  document.addEventListener("keydown", (event) => {
    if (event.key === "Enter" && isPasswordField(event.target)) capture(event.target.form || document);
  }, true);

  // ---- Telling Stillwater about sign-in forms ----

  // Each sign-in field is reported once, so a username-first page is reported again when its
  // password field appears.
  const reported = new WeakSet();
  function reportForms() {
    let fresh = false;
    for (const group of loginGroups()) {
      for (const field of [group.user, group.password]) {
        if (field && !reported.has(field)) {
          reported.add(field);
          fresh = true;
        }
      }
    }
    if (fresh) log(PREFIX + "form");
  }

  // ---- The saved-login list under a focused sign-in field ----

  let shownFor = null;
  function rectOf(field) {
    const rect = field.getBoundingClientRect();
    return stringify({ x: Math.round(rect.left), y: Math.round(rect.top),
      w: Math.round(rect.width), h: Math.round(rect.height) });
  }
  function offerList(field) {
    if (!(field instanceof HTMLInputElement) || !groupOf(field)) return;
    shownFor = field;
    log(PREFIX + "focus\t" + rectOf(field));
  }
  function hideList() {
    if (!shownFor) return;
    shownFor = null;
    log(PREFIX + "blur");
  }

  document.addEventListener("focusin", (event) => {
    reportForms();
    const field = focusedElement();
    if (field instanceof HTMLInputElement && groupOf(field)) offerList(field);
    else hideList();
  }, true);
  document.addEventListener("focusout", () => {
    // Focus moving between fields sends focusout then focusin; wait for the new field.
    setTimeout(() => { if (focusedElement() !== shownFor) hideList(); }, 0);
  }, true);
  // A click into the field that already has focus shows the list again.
  document.addEventListener("mousedown", (event) => {
    const field = event.composedPath ? event.composedPath()[0] : event.target;
    if (field === focusedElement()) offerList(field);
  }, true);
  // Clearing the field shows the list again.
  document.addEventListener("input", (event) => {
    const field = event.composedPath ? event.composedPath()[0] : event.target;
    if (field instanceof HTMLInputElement && field === focusedElement() && field.value === "") offerList(field);
  }, true);
  // Keep the list under its field while the page scrolls or resizes.
  let moving = false;
  function follow() {
    if (!shownFor || moving) return;
    moving = true;
    requestAnimationFrame(() => {
      moving = false;
      if (shownFor) log(PREFIX + "move\t" + rectOf(shownFor));
    });
  }
  window.addEventListener("scroll", follow, { capture: true, passive: true });
  window.addEventListener("resize", follow, { passive: true });

  // ---- Watching for sign-in forms that appear later ----

  function start() {
    reportForms();
    // Single-page sites add and swap sign-in forms long after loading: keep watching, at most
    // a few times a second.
    let pending = false;
    const observer = new MutationObserver(() => {
      if (pending) return;
      pending = true;
      setTimeout(() => { pending = false; reportForms(); }, 250);
    });
    observer.observe(document.documentElement, { childList: true, subtree: true, attributes: true, attributeFilter: ["type", "style", "class", "hidden", "disabled", "autocomplete"] });
    // The field may have been focused (autofocus) before this ran.
    const field = focusedElement();
    if (field instanceof HTMLInputElement && groupOf(field)) offerList(field);
  }

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", start, { once: true });
  } else {
    start();
  }
})();
