// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Stillwater passwords: tell Stillwater when the page shows a sign-in form (so it can fill a
// saved password) and which username and password were signed in with (so it can offer to save
// them). Stillwater fills forms itself (passwords-fill.js). Messages go through a tagged console message that Stillwater keeps out of its logs.
// Runs in top-level pages only.

(function () {
  "use strict";
  if (window !== window.top) return;
  // Keep our own references: pages sometimes replace these.
  const log = console.log.bind(console);
  const stringify = JSON.stringify;
  const PREFIX = "__PREFIX__";

  const USERNAME_TYPES = new Set(["text", "email", "tel", ""]);

  function isPassword(input) {
    return input instanceof HTMLInputElement && input.type === "password";
  }

  function visible(element) {
    return element.offsetWidth > 0 || element.offsetHeight > 0 || element.getClientRects().length > 0;
  }

  // The username field for a password field: one marked as a username, otherwise the closest
  // text-like field before it (in its form, or anywhere on the page when there's no form).
  function usernameFor(password, onlyFilled) {
    const scope = password.form || document;
    const inputs = Array.from(scope.querySelectorAll("input"));
    const before = inputs.slice(0, inputs.indexOf(password)).reverse();
    const candidates = before.filter((input) =>
      USERNAME_TYPES.has(input.type) && !input.disabled && (!onlyFilled || input.value.trim()));
    const marked = candidates.find((input) => /username|email/.test(input.autocomplete || ""));
    return marked || candidates[0] || null;
  }

  // ---- Telling Stillwater about sign-in forms ----

  let formReported = false;
  function reportForm() {
    if (formReported) return;
    const password = Array.from(document.querySelectorAll("input[type=password]")).find(visible);
    if (!password) return;
    formReported = true;
    log(PREFIX + "form");
  }

  // ---- Offering to save what was signed in with ----

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
    const user = usernameFor(password, true);
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
    if (event.key === "Enter" && isPassword(event.target)) capture(event.target.form || document);
  }, true);

  // ---- Watching for sign-in forms that appear later ----

  function start() {
    reportForm();
    if (formReported) return;
    let pending = false;
    const observer = new MutationObserver(() => {
      if (pending) return;
      pending = true;
      setTimeout(() => { pending = false; reportForm(); if (formReported) observer.disconnect(); }, 300);
    });
    observer.observe(document.documentElement, { childList: true, subtree: true, attributes: true, attributeFilter: ["type", "style", "class", "hidden"] });
    // Stop watching after a while to save CPU; a click on the page checks again.
    setTimeout(() => observer.disconnect(), 60000);
    document.addEventListener("focusin", (event) => { if (isPassword(event.target)) reportForm(); }, true);
  }

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", start, { once: true });
  } else {
    start();
  }
})();
