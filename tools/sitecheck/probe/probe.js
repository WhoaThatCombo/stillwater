// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// sitecheck probe, injected into every page via `servoshell --userscripts`.
// Reports what the page actually shows as console lines starting with "__sitecheck__",
// which sitecheck.py reads from Stillwater's output.

(function () {
  "use strict";
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
    if (errors.length < 30) errors.push("unhandled rejection: " + String(event.reason && (event.reason.message || event.reason)));
  });

  function report(phase) {
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
        // Web APIs pages commonly feature-detect; missing ones explain many breakages.
        missing: [
          "IntersectionObserver", "ResizeObserver", "MutationObserver", "customElements",
          "indexedDB", "Intl", "fetch", "WebSocket", "Worker", "BroadcastChannel",
          "structuredClone", "queueMicrotask", "requestIdleCallback", "CSSStyleSheet",
          "PerformanceObserver", "AbortController", "TextDecoder", "crypto",
        ].filter((name) => typeof window[name] === "undefined"),
      };
      console.log("__sitecheck__" + JSON.stringify(data));
    } catch (error) {
      console.log("__sitecheck__" + JSON.stringify({ phase, probeError: String(error) }));
    }
  }

  if (document.readyState === "complete") report("load");
  else window.addEventListener("load", () => report("load"));
  setTimeout(() => report("settled"), 8000);
})();
