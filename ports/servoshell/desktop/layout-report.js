// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// "Save layout report for this element" (page context menu): describes how the element under
// the pointer and its surroundings were laid out, for bug reports about boxes that render at
// the wrong size. The shell replaces __X__ and __Y__ with the point (CSS px in the page).
//
// Private by construction: no text of the page, no ids, no class names (only how many), no
// attribute values except data-testid (cut at its first "-", "_" + digit or ":" so no account
// names or ids get in) and role; URLs are cut to scheme, host and file extension. Read-only:
// it never changes the page. Evaluates to a string.

(function () {
  "use strict";
  const X = __X__, Y = __Y__;
  const MAX_DESCENDANTS = 400;
  const lines = [];
  const round = (n) => Math.round(n * 10) / 10;
  const cleanUrl = (value) => {
    if (!value) return "";
    try {
      const url = new URL(value, location.href);
      if (url.protocol === "data:") return (value.match(/^data:[\w.+-]{0,40}\/?[\w.+-]{0,40}/) || ["data:"])[0];
      if (url.protocol === "blob:") return "blob:";
      const ext = (url.pathname.match(/\.[A-Za-z0-9]{1,5}$/) || [""])[0];
      return url.protocol + "//" + url.host + (ext ? " [" + ext + "]" : "");
    } catch (e) {
      return "(unparsable)";
    }
  };
  const cleanTestId = (value) => {
    if (!value) return "";
    return value.split(/[-:]|_(?=\d)/)[0].replace(/[^A-Za-z0-9_]/g, "").slice(0, 40);
  };
  const PROPS = [
    "display", "position", "box-sizing", "width", "height", "min-width", "max-width",
    "min-height", "max-height", "aspect-ratio", "flex-direction", "flex-wrap", "flex-grow",
    "flex-shrink", "flex-basis", "align-items", "align-self", "justify-content", "top", "right",
    "bottom", "left", "margin-top", "margin-right", "margin-bottom", "margin-left", "padding-top",
    "padding-right", "padding-bottom", "padding-left", "border-top-width", "border-right-width",
    "border-bottom-width", "border-left-width", "overflow-x", "overflow-y", "object-fit",
    "transform", "opacity", "visibility", "z-index", "contain", "float", "pointer-events",
    "will-change", "clip-path", "backdrop-filter", "isolation", "mix-blend-mode",
    "grid-template-columns", "grid-template-rows",
  ];
  const DEFAULTS = {
    "box-sizing": "content-box", "min-width": "auto", "max-width": "none", "min-height": "auto",
    "max-height": "none", "aspect-ratio": "auto", "flex-wrap": "nowrap", "flex-grow": "0",
    "flex-shrink": "1", "flex-basis": "auto", "align-self": "auto", "top": "auto",
    "right": "auto", "bottom": "auto", "left": "auto", "object-fit": "fill", "transform": "none",
    "opacity": "1", "visibility": "visible", "z-index": "auto", "contain": "none",
    "float": "none", "grid-template-columns": "none", "grid-template-rows": "none",
    "pointer-events": "auto", "will-change": "auto", "clip-path": "none",
    "backdrop-filter": "none", "isolation": "auto", "mix-blend-mode": "normal",
  };
  const styleOf = (style, all) => PROPS
    .map((name) => [name, style.getPropertyValue(name)])
    .filter(([name, value]) => value !== "")
    .filter(([name, value]) => all || !(name in DEFAULTS) || DEFAULTS[name] !== value)
    .filter(([name, value]) => !/^(margin|padding|border)-/.test(name) || (value !== "0px"))
    .map(([name, value]) => name + "=" + value.replace(/url\([^)]*\)/g, "url(…)"))
    .join(" ");
  const tagOf = (element) => {
    let tag = element.localName;
    const testId = cleanTestId(element.getAttribute("data-testid"));
    if (testId) tag += '[data-testid="' + testId + '"]';
    const role = element.getAttribute("role");
    if (role && /^[a-z]{1,20}$/.test(role)) tag += '[role="' + role + '"]';
    if (element.classList.length) tag += " (" + element.classList.length + " classes)";
    return tag;
  };
  const rectOf = (element) => {
    const r = element.getBoundingClientRect();
    return "rect=" + round(r.left) + "," + round(r.top) + " " + round(r.width) + "x" + round(r.height);
  };
  const mediaOf = (element) => {
    const parts = [];
    for (const name of ["src", "href", "poster"]) {
      if (element.hasAttribute(name)) parts.push(name + "=" + cleanUrl(element.getAttribute(name)));
    }
    if (element instanceof HTMLImageElement) {
      parts.push("natural=" + element.naturalWidth + "x" + element.naturalHeight, "complete=" + element.complete);
      if (element.currentSrc) parts.push("currentSrc=" + cleanUrl(element.currentSrc));
    }
    if (element instanceof HTMLMediaElement) {
      parts.push("currentSrc=" + cleanUrl(element.currentSrc), "readyState=" + element.readyState,
        "networkState=" + element.networkState, "paused=" + element.paused,
        "loop=" + element.loop, "muted=" + element.muted, "autoplay=" + element.autoplay,
        "controls=" + element.controls, "error=" + (element.error ? element.error.code : "none"));
      if (element instanceof HTMLVideoElement) parts.push("video=" + element.videoWidth + "x" + element.videoHeight);
    }
    for (const name of ["width", "height"]) {
      const value = element.getAttribute(name);
      if (value !== null && /^[0-9.%a-z ]{0,12}$/.test(value)) parts.push("attr-" + name + "=" + value);
    }
    return parts.join(" ");
  };
  const pseudoOf = (element, which) => {
    const style = getComputedStyle(element, which);
    const content = style.getPropertyValue("content");
    if (!content || content === "none" || content === "normal") return null;
    return which + " " + styleOf(style, false);
  };
  const describe = (element, indent, all) => {
    const style = getComputedStyle(element);
    const pad = "  ".repeat(indent);
    lines.push(pad + tagOf(element) + " " + rectOf(element) + (mediaOf(element) ? " " + mediaOf(element) : ""));
    lines.push(pad + "    " + styleOf(style, all));
    for (const which of ["::before", "::after"]) {
      const pseudo = pseudoOf(element, which);
      if (pseudo) lines.push(pad + "    " + pseudo);
    }
  };

  lines.push("Stillwater layout report");
  lines.push("viewport=" + innerWidth + "x" + innerHeight + " devicePixelRatio=" + devicePixelRatio +
    " scroll=" + round(scrollX) + "," + round(scrollY) + " point=" + round(X) + "," + round(Y) +
    " page=" + cleanUrl(location.href) + " color-scheme=" +
    (matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light"));

  // Sites with their own context menu (X on media) may have opened it at the point by now:
  // skip popups and fixed layers to get to the element that was right-clicked.
  const atPoint = document.elementsFromPoint(X, Y);
  const isPopup = (element) => {
    for (let node = element; node && node !== document.body; node = node.parentElement) {
      if (/^(menu|menuitem|presentation|dialog|tooltip|listbox)$/.test(node.getAttribute("role") || "")) return true;
      if (getComputedStyle(node).position === "fixed") return true;
    }
    return false;
  };
  const target = atPoint.find((element) => !isPopup(element)) || atPoint[0];
  if (!target) {
    lines.push("No element at the point.");
    return lines.join("\n");
  }

  lines.push("", "== Everything at the point, topmost first");
  for (const element of atPoint.slice(0, 40)) {
    lines.push("  " + (element === target ? "> " : "") + tagOf(element) + " " + rectOf(element));
  }

  // Boxes that cover the point but that the hit test passed over: an overlay that is drawn
  // there but doesn't take clicks shows up here (deepest first, with its ancestors).
  const hit = new Set(atPoint);
  const depthOf = (element) => {
    let depth = 0;
    for (let e = element; e; e = e.parentElement) depth++;
    return depth;
  };
  const skipped = [];
  for (const element of document.querySelectorAll("body *")) {
    if (hit.has(element)) continue;
    const rect = element.getBoundingClientRect();
    if (rect.width <= 0 || rect.height <= 0) continue;
    if (X < rect.left || X > rect.right || Y < rect.top || Y > rect.bottom) continue;
    skipped.push(element);
    if (skipped.length > 2000) break;
  }
  skipped.sort((a, b) => depthOf(b) - depthOf(a));
  lines.push("", "== Boxes covering the point that the hit test passed over (" + skipped.length + ", deepest first)");
  const shownChains = new Set();
  for (const element of skipped.slice(0, 6)) {
    const chain = [];
    for (let e = element; e && chain.length < 10; e = e.parentElement) chain.push(e);
    const key = chain.map(tagOf).join("<");
    if (shownChains.has(key)) continue;
    shownChains.add(key);
    lines.push("-- " + tagOf(element) + " and its ancestors (innermost first)");
    chain.forEach((e) => describe(e, 1, false));
  }

  // Why the boxes above don't take clicks: the style rules that set pointer-events on the
  // first of them and its ancestors (selectors are the site's own CSS, not page content).
  if (skipped.length) {
    const rules = [];
    const collect = (list) => {
      for (const rule of list) {
        if (rule.cssRules && !rule.selectorText) {
          try { collect(rule.cssRules); } catch (e) {}
        } else if (rule.selectorText && rule.style && rule.style.getPropertyValue("pointer-events")) {
          rules.push(rule);
        }
      }
    };
    for (const sheet of document.styleSheets) {
      try { collect(sheet.cssRules); } catch (e) {}
    }
    lines.push("", "== pointer-events rules on the first skipped box and its ancestors (innermost first)");
    const chain = [];
    for (let e = skipped[0]; e && e !== document.documentElement && chain.length < 30; e = e.parentElement) chain.push(e);
    for (const element of chain) {
      const found = [];
      for (const rule of rules) {
        let matches = false;
        try { matches = element.matches(rule.selectorText); } catch (e) {}
        if (matches) {
          found.push(rule.selectorText.slice(0, 120) + " { pointer-events: " +
            rule.style.getPropertyValue("pointer-events") +
            (rule.style.getPropertyPriority("pointer-events") ? " !important" : "") + " }");
        }
      }
      const inline = element.style && element.style.getPropertyValue("pointer-events");
      if (inline) found.push("style attribute { pointer-events: " + inline + " }");
      lines.push("  " + tagOf(element) + " " + rectOf(element) + " computed=" +
        getComputedStyle(element).getPropertyValue("pointer-events"));
      for (const line of found) lines.push("      " + line);
    }
  }

  lines.push("", "== The element under the pointer (marked > above) and its 12 nearest ancestors (outermost first)");
  const chain = [];
  for (let element = target; element && chain.length < 13; element = element.parentElement) chain.push(element);
  chain.reverse().forEach((element, index) => describe(element, index, index === chain.length - 1));

  // Descendants: of the element under the pointer, and of the nearest ancestor holding media
  // (the pointer is often on an overlay drawn above the picture or video).
  let count = 0;
  const walk = (element, depth, indent) => {
    if (depth > 6 || count >= MAX_DESCENDANTS) return;
    for (let child = element.firstElementChild; child && count < MAX_DESCENDANTS; child = child.nextElementSibling) {
      count++;
      describe(child, indent, false);
      walk(child, depth + 1, indent + 1);
    }
  };
  lines.push("", "== Descendants of the element under the pointer (depth 6)");
  walk(target, 1, 1);
  // A video first: it may have collapsed away from the point.
  const innermostFirst = chain.slice().reverse().filter((element) => element !== target &&
    element !== document.body && element !== document.documentElement);
  const media = innermostFirst.find((element) => element.querySelector("video")) ||
    innermostFirst.find((element) => element.querySelector("img, canvas, picture, iframe"));
  if (media) {
    lines.push("", "== Descendants of the nearest ancestor holding media: " + tagOf(media) + " " + rectOf(media));
    walk(media, 1, 1);
  }
  if (count >= MAX_DESCENDANTS) lines.push("(stopped after " + MAX_DESCENDANTS + " descendants)");
  return lines.join("\n");
})()
