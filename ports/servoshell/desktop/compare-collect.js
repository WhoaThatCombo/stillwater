// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Measures the page for comparing Stillwater with another browser (Compare with Edge, and
// tools/sitecheck/compare.py): every visible element on the first two screens, with its box,
// text size, weight, font and colours. Elements are keyed by their path from <body> (tag names
// and positions among same-tag siblings), so the same element can be found in both browsers.
// Read-only: it never changes the page. Evaluates to a JSON string.

(function () {
  "use strict";
  const MAX_ELEMENTS = 3000;
  const bottom = innerHeight * 2;
  const keyOf = (element) => {
    const parts = [];
    for (let node = element; node && node !== document.documentElement && parts.length < 80; node = node.parentElement) {
      let index = 1;
      for (let sibling = node.previousElementSibling; sibling; sibling = sibling.previousElementSibling) {
        if (sibling.tagName === node.tagName) index++;
      }
      parts.push(node.tagName.toLowerCase() + (index > 1 ? ":" + index : ""));
    }
    return parts.reverse().join(">");
  };
  const elements = [];
  const body = document.body;
  if (body) {
    const walker = document.createTreeWalker(body, NodeFilter.SHOW_ELEMENT);
    for (let element = walker.currentNode; element && elements.length < MAX_ELEMENTS; element = walker.nextNode()) {
      const tag = element.tagName.toLowerCase();
      if (tag === "script" || tag === "style" || tag === "noscript" || tag === "template") continue;
      // An SVG image counts as one element; its shapes aren't compared.
      if (element.ownerSVGElement) continue;
      const box = element.getBoundingClientRect();
      if (box.width < 1 || box.height < 1 || box.top > bottom || box.bottom < 0) continue;
      const style = getComputedStyle(element);
      if (style.visibility === "hidden" || style.display === "none" || Number(style.opacity) === 0) continue;
      let text = "";
      for (const child of element.childNodes) {
        if (child.nodeType === Node.TEXT_NODE) text += child.nodeValue;
      }
      text = text.replace(/\s+/g, " ").trim().slice(0, 80);
      elements.push({
        k: keyOf(element),
        t: tag,
        x: Math.round(box.left + scrollX),
        y: Math.round(box.top + scrollY),
        w: Math.round(box.width),
        h: Math.round(box.height),
        fs: style.fontSize,
        fw: style.fontWeight,
        ff: (style.fontFamily || "").split(",")[0].replace(/["']/g, "").trim(),
        c: style.color,
        bg: style.backgroundColor,
        txt: text,
      });
    }
  }
  return JSON.stringify({
    url: location.href,
    title: document.title,
    viewport: [innerWidth, innerHeight],
    scrollHeight: document.documentElement ? document.documentElement.scrollHeight : 0,
    count: document.getElementsByTagName("*").length,
    elements,
  });
})()
