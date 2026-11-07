// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// sitecheck probe, injected into every page via `servoshell --userscripts`.
// Reports what the page actually shows. sitecheck.py asks for the report over WebDriver
// (window.__sitecheck); with --legacy it reads console lines starting with "__sitecheck__"
// from Stillwater's output instead.

(function () {
  "use strict";
  // Keep our own reference: some sites (apnews.com, duolingo.com) replace console.log to silence it.
  const log = console.log.bind(console);
  if (window.top !== window) return; // main frame only

  // Text that anti-bot / "unsupported browser" pages show.
  const BLOCK_PHRASES = [
    "checking your browser", "just a moment", "verify you are human", "are you a robot",
    "unsupported browser", "browser is not supported", "update your browser",
    "enable javascript", "something went wrong", "access denied", "attention required",
    "please wait while we", "security check", "captcha",
  ];

  const errors = [];
  window.addEventListener("error", (event) => {
    if (errors.length < 30) errors.push(String(event.message || event.error || "error"));
  });
  window.addEventListener("unhandledrejection", (event) => {
    const reason = event.reason;
    // Rejections carry no location in Stillwater's output, so keep where the reason was created.
    const where = reason && typeof reason.stack === "string" ? reason.stack.split("\n")[0] : "";
    if (errors.length < 30) errors.push("unhandled rejection: " + String(reason && (reason.message || reason)) + (where ? " @ " + where : ""));
  });

  // Boxes much wider than the window that aren't clipped down to it. A real browser almost
  // never shows these; in Stillwater they usually mean unsupported CSS let an image or video
  // fall back to its natural size (GitHub's 2561px hero video, for one).
  function oversized() {
    const limit = window.innerWidth * 1.3;
    const found = [];
    // Big pages (10,000+ elements) can take seconds to measure; look at what fits in 2 s.
    const deadline = performance.now() + 2000;
    // Width each ancestor clips its content to (Infinity if it doesn't), measured once each.
    const clips = new Map();
    function clip(element) {
      let width = clips.get(element);
      if (width === undefined) {
        width = getComputedStyle(element).overflowX !== "visible" ? element.getBoundingClientRect().width : Infinity;
        clips.set(element, width);
      }
      return width;
    }
    for (const element of document.querySelectorAll("body *")) {
      if (found.length >= 5 || performance.now() > deadline) break;
      const rect = element.getBoundingClientRect();
      if (rect.width <= limit || rect.bottom < 0 || rect.top > window.innerHeight * 3) continue;
      let visible = rect.width;
      for (let parent = element.parentElement; parent && parent !== document.body; parent = parent.parentElement) {
        visible = Math.min(visible, clip(parent));
      }
      if (visible <= window.innerWidth * 1.1) continue;
      const style = getComputedStyle(element);
      if (style.visibility === "hidden" || style.opacity === "0") continue;
      const name = element.tagName.toLowerCase() + (element.classList.length ? "." + element.classList[0] : "");
      found.push(name.slice(0, 60) + " " + Math.round(rect.width) + "px");
    }
    return found;
  }

  function collect(phase) {
    try {
      const body = document.body;
      const text = body ? (body.innerText || body.textContent || "") : "";
      const lower = text.slice(0, 20000).toLowerCase();
      const blocked = BLOCK_PHRASES.filter((phrase) => lower.includes(phrase));
      const data = {
        phase,
        url: location.href,
        title: document.title,
        readyState: document.readyState,
        textLength: text.trim().length,
        elements: document.getElementsByTagName("*").length,
        images: document.images.length,
        links: document.links.length,
        blocked,
        errors: errors.slice(0, 15),
        oversized: oversized(),
        scrollWidth: document.documentElement ? document.documentElement.scrollWidth : 0,
        // Web APIs pages commonly feature-detect; missing ones explain many breakages.
        missing: [
          "IntersectionObserver", "ResizeObserver", "MutationObserver", "customElements",
          "indexedDB", "Intl", "fetch", "WebSocket", "Worker", "BroadcastChannel",
          "structuredClone", "queueMicrotask", "requestIdleCallback", "CSSStyleSheet",
          "PerformanceObserver", "AbortController", "TextDecoder", "crypto",
        ].filter((name) => typeof window[name] === "undefined"),
      };
      return data;
    } catch (error) {
      return { phase, probeError: String(error) };
    }
  }

  function report(phase) {
    log("__sitecheck__" + JSON.stringify(collect(phase)));
  }

  // sitecheck's WebDriver mode asks for the report itself (`return window.__sitecheck("settled")`).
  Object.defineProperty(window, "__sitecheck", { value: collect });

  if (document.readyState === "complete") report("load");
  else window.addEventListener("load", () => report("load"));
  setTimeout(() => report("settled"), 8000);
})();
