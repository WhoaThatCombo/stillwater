  // ---- Finding sign-in fields (shared by passwords.js and passwords-fill.js) ----
  //
  // A sign-in form is one visible, editable password field in its form (or among the fields
  // outside any form), with the username field before it. Forms with two or more password
  // fields are sign-up or change-password forms, and `autocomplete="new-password"` fields are
  // for new passwords: neither is filled. Some sites ask for the username first, on a page of
  // its own: a lone username field (marked `autocomplete="username"`, or named like one) counts.

  const USERNAME_TYPES = ["text", "email", "tel", ""];
  const attribute = (element, name) => (element.getAttribute(name) || "").toLowerCase();

  function visible(element) {
    if (!(element.offsetWidth > 0 || element.offsetHeight > 0) || element.getClientRects().length === 0) return false;
    const style = getComputedStyle(element);
    return style.visibility !== "hidden" && style.display !== "none" && Number(style.opacity) !== 0;
  }

  function editable(input) {
    return !input.disabled && !input.readOnly && visible(input);
  }

  function isPasswordField(input) {
    return input instanceof HTMLInputElement && input.type === "password";
  }

  function isTextLike(input) {
    return input instanceof HTMLInputElement && USERNAME_TYPES.includes(input.type) &&
      !/one-time-code|new-password|current-password|cc-|search/.test(attribute(input, "autocomplete"));
  }

  const NOT_USERNAME = /search|captcha|otp|one.?time|verif|coupon|promo|zip|postal/i;
  const SIGN_UP = /sign.?up|regist|create.?account|new.?account|join/i;
  const LOGIN_NAME = /^(user(.?name)?|login(.?id|.?name)?|loginfmt|identifier(id)?|account(.?name)?|email.?or.?(phone|user(name)?)|username.?or.?email|session\[username_or_email\])$/i;

  function describe(element) {
    return element ? [element.id, element.getAttribute("name"), element.getAttribute("action"), element.className]
      .filter((part) => typeof part === "string").join(" ") : "";
  }

  // The username field for `password`: one marked as a username, otherwise the closest
  // editable text-like field before it in the same form (or outside any form).
  function usernameFor(password) {
    const form = password.form;
    const inputs = Array.from((form || document).querySelectorAll("input"))
      .filter((input) => input.form === form);
    const before = inputs.slice(0, inputs.indexOf(password)).reverse()
      .filter((input) => isTextLike(input) && editable(input) && !NOT_USERNAME.test(describe(input)));
    return before.find((input) => /username|email/.test(attribute(input, "autocomplete"))) || before[0] || null;
  }

  // A username field on its own (the first step of a two-step sign-in).
  function lonelyUsername(input) {
    if (!isTextLike(input) || !editable(input)) return false;
    const autocomplete = attribute(input, "autocomplete");
    if (/\busername\b/.test(autocomplete)) return true;
    if (SIGN_UP.test(describe(input.form))) return false;
    if (!LOGIN_NAME.test(input.getAttribute("name") || "") && !LOGIN_NAME.test(input.id || "")) return false;
    // Named like a username: only in a small form (not a profile or contact form).
    const others = Array.from((input.form || document).querySelectorAll("input"))
      .filter((other) => other !== input && other.form === input.form && isTextLike(other) && editable(other));
    return others.length === 0;
  }

  // The sign-in fields on the page: a list of { user, password } (password is null on a
  // username-only step).
  function loginGroups() {
    const inputs = Array.from(document.querySelectorAll("input"));
    const byForm = new Map();
    for (const input of inputs) {
      if (!isPasswordField(input) || !visible(input)) continue;
      const list = byForm.get(input.form) || [];
      list.push(input);
      byForm.set(input.form, list);
    }
    const groups = [];
    for (const [form, list] of byForm) {
      if (list.length !== 1) continue;
      const password = list[0];
      if (!editable(password) || /new-password/.test(attribute(password, "autocomplete"))) continue;
      if (SIGN_UP.test(describe(form)) && !/current-password/.test(attribute(password, "autocomplete"))) continue;
      groups.push({ user: usernameFor(password), password });
    }
    if (groups.length === 0 && byForm.size === 0) {
      const user = inputs.find(lonelyUsername);
      if (user) groups.push({ user, password: null });
    }
    return groups;
  }

  // The focused element, looking inside shadow roots.
  function focusedElement() {
    let element = document.activeElement;
    while (element && element.shadowRoot && element.shadowRoot.activeElement) {
      element = element.shadowRoot.activeElement;
    }
    return element;
  }

  function groupOf(field) {
    return loginGroups().find((group) => group.user === field || group.password === field) || null;
  }
