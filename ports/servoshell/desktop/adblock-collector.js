// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Stillwater adblock: report the CSS classes and ids used on the page, so Stillwater can hide
// elements that generic cosmetic filters (e.g. `##.ad-banner`) target. Runs in top-level pages
// only, reports new names in batches, and stops watching after a while to save CPU.

(function () {
  "use strict";
  if (window !== window.top) return;
  // Keep our own reference: pages sometimes replace console.log.
  const log = console.log.bind(console);
  const seenClasses = new Set();
  const seenIds = new Set();
  let classes = [];
  let ids = [];

  function note(element) {
    const id = element.id;
    if (id && typeof id === "string" && !seenIds.has(id)) {
      seenIds.add(id);
      ids.push(id);
    }
    const list = element.classList;
    if (!list) return;
    for (const name of list) {
      if (!seenClasses.has(name)) {
        seenClasses.add(name);
        classes.push(name);
      }
    }
  }

  function scan(root) {
    if (!root || root.nodeType !== 1) return;
    note(root);
    for (const element of root.querySelectorAll("[class],[id]")) note(element);
  }

  function flush() {
    if (classes.length === 0 && ids.length === 0) return;
    log("__PREFIX__" + JSON.stringify([classes, ids]));
    classes = [];
    ids = [];
  }

  function start() {
    scan(document.documentElement);
    flush();
    const pending = [];
    const observer = new MutationObserver((mutations) => {
      for (const mutation of mutations) {
        for (const node of mutation.addedNodes) pending.push(node);
      }
    });
    observer.observe(document.documentElement, { childList: true, subtree: true });
    let ticks = 0;
    const timer = setInterval(() => {
      for (const node of pending.splice(0)) scan(node);
      flush();
      if (++ticks >= 30) {
        observer.disconnect();
        clearInterval(timer);
      }
    }, 1000);
  }

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", start, { once: true });
  } else {
    start();
  }
})();
