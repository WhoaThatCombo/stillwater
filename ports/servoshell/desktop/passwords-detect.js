  // ---- Finding sign-in fields (shared by passwords.js and passwords-fill.js) ----
  //
  // A sign-in form is one visible, editable password field in its form (or among the fields
  // outside any form), with the username field before it. Forms with two or more password
  // fields are sign-up or change-password forms, and `autocomplete="new-password"` fields are
  // for new passwords: neither is filled. Some sites ask for the username first, on a page of
  // its own: a lone username field (marked `autocomplete="username"`, or named like one) counts.
  // Fields inside open shadow roots (web components, e.g. Reddit's) count too.

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
  const LOGIN_NAME = /^(user(.?name)?|login(.?id|.?name|.?user(.?name)?|.?email)?|loginfmt|identifier(id)?|account(.?name)?|email.?or.?(phone|user(name)?)|username.?or.?email|session\[username_or_email\]|sign.?in.?(email|user(.?name)?))$/i;

  function describe(element) {
    return element ? [element.id, element.getAttribute("name"), element.getAttribute("action"), element.className]
      .filter((part) => typeof part === "string").join(" ") : "";
  }

  // Open shadow roots found by the last `allInputs()` (passwords.js watches them for changes).
  const shadowRoots = new Set();
  let shadowInputs = 0;

  // Every <input> on the page in document order, including those inside open shadow roots
  // (placed where their host is). Only walks into shadow roots when the page has any.
  let lastInputs = [];
  function allInputs() {
    shadowInputs = 0;
    shadowRoots.clear();
    if (!Array.prototype.some.call(document.querySelectorAll("*"), (element) => element.shadowRoot)) {
      return (lastInputs = Array.from(document.querySelectorAll("input")));
    }
    const out = lastInputs = [];
    const walk = (root, inShadow) => {
      const walker = document.createTreeWalker(root, NodeFilter.SHOW_ELEMENT);
      for (let node = walker.nextNode(); node; node = walker.nextNode()) {
        if (node instanceof HTMLInputElement) {
          out.push(node);
          if (inShadow) shadowInputs++;
        }
        if (node.shadowRoot) {
          shadowRoots.add(node.shadowRoot);
          walk(node.shadowRoot, true);
        }
      }
    };
    walk(document, false);
    return out;
  }

  // The username field for `password`: one marked as a username, otherwise the closest
  // editable text-like field before it in the same form (or outside any form).
  function usernameFor(password, inputs) {
    const form = password.form;
    const same = inputs.filter((input) => input.form === form);
    const before = same.slice(0, same.indexOf(password)).reverse()
      .filter((input) => isTextLike(input) && editable(input) && !NOT_USERNAME.test(describe(input)));
    return before.find((input) => /username|email/.test(attribute(input, "autocomplete"))) || before[0] || null;
  }

  // A username field on its own (the first step of a two-step sign-in). `passwords` are the
  // visible password fields: a form that has one isn't a username-only step.
  function lonelyUsername(input, inputs, passwords) {
    if (!isTextLike(input) || !editable(input)) return false;
    if (passwords.some((password) => password.form === input.form && editable(password))) return false;
    const autocomplete = attribute(input, "autocomplete");
    if (/\busername\b/.test(autocomplete)) return true;
    if (SIGN_UP.test(describe(input.form))) return false;
    const named = LOGIN_NAME.test(input.getAttribute("name") || "") || LOGIN_NAME.test(input.id || "");
    // An e-mail field (marked, typed or named as one) in a form that looks like a sign-in form,
    // e.g. Amazon's `<form name="signIn"><input name="email">`.
    const emailish = /\bemail\b/.test(autocomplete) || input.type === "email" ||
      /e.?mail/i.test((input.getAttribute("name") || "") + " " + (input.id || ""));
    const email = emailish && /log.?in|sign.?in|auth/i.test(describe(input.form) + " " + location.pathname);
    if (!named && !email) return false;
    // Named like a username: only in a small form (not a profile or contact form).
    const others = inputs.filter((other) => other !== input && other.form === input.form && isTextLike(other) && editable(other));
    return others.length === 0;
  }

  // The sign-in fields on the page: a list of { user, password } (password is null on a
  // username-only step).
  function loginGroups() {
    const inputs = allInputs();
    const passwords = [];
    const byForm = new Map();
    for (const input of inputs) {
      if (!isPasswordField(input) || !visible(input)) continue;
      passwords.push(input);
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
      groups.push({ user: usernameFor(password, inputs), password });
    }
    // No usable password field (or only decoys that can't be typed in, like Google's hidden
    // one): a username-only step, if there is one.
    if (groups.length === 0) {
      const user = inputs.find((input) => lonelyUsername(input, inputs, passwords));
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
