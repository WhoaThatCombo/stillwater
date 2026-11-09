/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

// Page translation, inside the page (desktop/translate.rs drives it with evaluate_javascript).
//
// The page's text is handed out in blocks: a paragraph, heading, list item or button, as the
// text nodes it's made of ("runs"), so a link's words can go back into the link. Blocks in view
// go first. Translations are applied only to text the page hasn't changed since, the originals
// are kept for "Show original", and text the page adds later (infinite scroll, menus) is picked
// up on the next collect.
(() => {
  if (window.__stillwaterTranslate) {
    return;
  }
  const SKIP = new Set([
    "SCRIPT", "STYLE", "NOSCRIPT", "TEMPLATE", "CODE", "PRE", "KBD", "SAMP", "VAR", "TEXTAREA",
    "INPUT", "SELECT", "OPTION", "SVG", "MATH", "CANVAS", "IFRAME", "OBJECT", "EMBED", "VIDEO",
    "AUDIO", "HEAD", "TITLE",
  ]);
  // Elements that are part of the sentence around them.
  const INLINE = new Set([
    "A", "ABBR", "B", "BDI", "BDO", "CITE", "DATA", "DFN", "EM", "FONT", "I", "LABEL", "MARK", "Q",
    "RUBY", "RT", "S", "SMALL", "SPAN", "STRONG", "SUB", "SUP", "TIME", "U", "BIG", "TT", "STRIKE",
    "DEL", "INS", "WBR", "BR", "IMG",
  ]);
  const ATTRIBUTES = ["title", "alt", "placeholder", "aria-label"];
  const LETTERS = /\p{L}/u;
  const BATCH_CHARACTERS = 5000;

  const originals = new Map();      // Text node -> its text before we translated it
  const translatedText = new WeakMap(); // Text node -> what we wrote, to notice page changes
  const attributeOriginals = [];    // [element, attribute, original]
  const handled = new WeakSet();    // text nodes / elements sent or done
  const handledAttributes = new WeakMap(); // element -> Set of attributes sent
  const pending = new Map();        // block id -> { nodes, sent } or { element, attribute, sent } or { title }
  let nextId = 1;
  let dirty = true;
  let titleOriginal = null;
  let titleSent = false;
  let lastShape = "";
  // Which round of Stillwater's this is: text handed out in an earlier round that never came
  // back is handed out again.
  let round = null;
  // Right-to-left pages read left to right once translated into, say, English (and back).
  const RTL = new Set(["ar", "he", "fa", "ur", "ug"]);
  const directionOriginals = [];
  let directionSet = false;

  const observer = new MutationObserver(() => { dirty = true; });

  function skipped(element) {
    for (let node = element; node && node.nodeType === 1; node = node.parentElement) {
      if (SKIP.has(node.tagName) || node.isContentEditable || node.getAttribute("translate") === "no" ||
          node.classList.contains("notranslate")) {
        return true;
      }
    }
    return false;
  }

  function blockOf(node) {
    let element = node.parentElement;
    while (element && INLINE.has(element.tagName) && element.parentElement) {
      element = element.parentElement;
    }
    return element;
  }

  function changedByPage(node) {
    return translatedText.has(node) && translatedText.get(node) !== node.data;
  }

  // Gathers new blocks: Map block element -> its text nodes, in document order.
  function gather() {
    const blocks = new Map();
    if (!document.body) {
      return blocks;
    }
    const walker = document.createTreeWalker(document.body, NodeFilter.SHOW_TEXT);
    const skipCache = new Map();
    for (let node = walker.nextNode(); node; node = walker.nextNode()) {
      if (handled.has(node) && !changedByPage(node)) {
        continue;
      }
      const parent = node.parentElement;
      if (!parent) {
        continue;
      }
      let skip = skipCache.get(parent);
      if (skip === undefined) {
        skip = skipped(parent);
        skipCache.set(parent, skip);
      }
      if (skip) {
        continue;
      }
      const block = blockOf(node);
      if (!block) {
        continue;
      }
      let nodes = blocks.get(block);
      if (!nodes) {
        nodes = [];
        blocks.set(block, nodes);
      }
      nodes.push(node);
    }
    return blocks;
  }

  // The text nodes of `block` that belong to it (not to a nested block), for a full set of runs
  // including whitespace between links.
  // Text inside a `translate="no"` element is sent for context but never changed.
  const keep = new WeakSet();
  function runsOf(block, seed) {
    const nodes = [];
    const walker = document.createTreeWalker(block, NodeFilter.SHOW_TEXT);
    for (let node = walker.nextNode(); node; node = walker.nextNode()) {
      if (blockOf(node) === block && (seed.includes(node) || !handled.has(node) || changedByPage(node))) {
        if (node.parentElement && skipped(node.parentElement)) {
          if (SKIP.has(node.parentElement.tagName)) continue;
          keep.add(node);
        }
        nodes.push(node);
      }
    }
    return nodes;
  }

  function inView(element) {
    const box = element.getBoundingClientRect();
    return box.bottom > -200 && box.top < innerHeight + 400 && box.width > 0 && box.height > 0;
  }

  // Blocks found by the last scan of the page and not handed out yet (visible ones first), and
  // whether the attributes still need a pass.
  let queue = [];
  let attributesDone = false;

  function scan() {
    const found = [...gather()];
    // What's on screen first, then the rest in page order.
    queue = [...found.filter(([block]) => inView(block)), ...found.filter(([block]) => !inView(block))];
    attributesDone = false;
    dirty = false;
  }

  // Hands out up to `budget` characters of text not translated yet. The page is scanned again
  // only when it changed; otherwise this carries on through the last scan's queue.
  function collect(newRound, budget = BATCH_CHARACTERS) {
    if (newRound !== undefined && newRound !== round) {
      if (round !== null) {
        for (const block of pending.values()) {
          if (block.nodes) block.nodes.forEach((node) => handled.delete(node));
          else if (block.element) (handledAttributes.get(block.element) || new Set()).delete(block.attribute);
          else if (block.title) titleSent = false;
        }
        pending.clear();
        dirty = true;
      }
      round = newRound;
    }
    const out = [];
    let characters = 0;
    if (!titleSent && document.title && LETTERS.test(document.title)) {
      titleSent = true;
      titleOriginal = document.title;
      const id = nextId++;
      pending.set(id, { title: true, sent: [document.title] });
      out.push({ i: id, r: [document.title] });
    }
    // A page that rebuilt its document (document.open) has a new body we aren't watching.
    if (observer.active && observer.target !== document.body) {
      observer.disconnect();
      observer.active = false;
      dirty = true;
    }
    // Content the HTML parser adds isn't reported to the observer; the number of elements (which
    // translating never changes) and the load state catch it.
    const shape = `${document.readyState}:${document.getElementsByTagName("*").length}`;
    if (shape !== lastShape) {
      lastShape = shape;
      dirty = true;
    }
    if (dirty) {
      scan();
    }
    while (queue.length && characters < budget) {
      const [block, seed] = queue.shift();
      const nodes = runsOf(block, seed);
      const runs = nodes.map((node) => node.data);
      for (const node of nodes) {
        handled.add(node);
      }
      if (!LETTERS.test(runs.join(""))) {
        continue;
      }
      // A block that's only links (a menu: Top | Society | Economy) isn't a sentence: each link
      // is translated on its own.
      const worded = nodes.filter((node) => LETTERS.test(node.data));
      const links = worded.map((node) => {
        const link = node.parentElement && node.parentElement.closest("a, button");
        return link && link !== block && block.contains(link) ? link : null;
      });
      if (worded.length > 1 && links.every(Boolean) && new Set(links).size === links.length) {
        for (const node of worded) {
          const id = nextId++;
          pending.set(id, { nodes: [node], sent: [node.data] });
          out.push({ i: id, r: [node.data] });
          characters += node.data.length;
        }
        continue;
      }
      const id = nextId++;
      pending.set(id, { nodes, sent: runs });
      out.push({ i: id, r: runs });
      characters += runs.reduce((total, run) => total + run.length, 0);
    }
    // Attributes people read: tooltips, image descriptions, placeholders, labels, button text.
    if (!queue.length && !attributesDone && characters < budget && document.body) {
      attributesDone = true;
      const selector = ATTRIBUTES.map((name) => `[${name}]`).join(",") +
        ',input[type="submit"][value],input[type="button"][value],input[type="reset"][value]';
      for (const element of document.body.querySelectorAll(selector)) {
        if (skipped(element.parentElement || element)) {
          continue;
        }
        let sent = handledAttributes.get(element);
        if (!sent) {
          sent = new Set();
          handledAttributes.set(element, sent);
        }
        const names = element.tagName === "INPUT" ? [...ATTRIBUTES, "value"] : ATTRIBUTES;
        for (const name of names) {
          const value = element.getAttribute(name);
          if (!value || sent.has(name) || !LETTERS.test(value) || (name === "value" && !/^(submit|button|reset)$/i.test(element.type))) {
            continue;
          }
          sent.add(name);
          const id = nextId++;
          pending.set(id, { element, attribute: name, sent: [value] });
          out.push({ i: id, r: [value] });
          characters += value.length;
        }
      }
    }
    if (!observer.active && document.body) {
      observer.observe(document.body, { childList: true, subtree: true, characterData: true });
      observer.active = true;
      observer.target = document.body;
    }
    return JSON.stringify(out);
  }

  function setDirection(target) {
    if (directionSet || !target || !document.body) return;
    directionSet = true;
    const wanted = RTL.has(target.split("-")[0]) ? "rtl" : "ltr";
    if (getComputedStyle(document.body).direction === wanted) return;
    const root = document.documentElement;
    for (const element of [root, ...document.querySelectorAll("[dir]")]) {
      const dir = element.getAttribute("dir");
      if (element === root || (dir && dir.toLowerCase() !== wanted && dir.toLowerCase() !== "auto")) {
        directionOriginals.push([element, "dir", dir]);
        element.setAttribute("dir", wanted);
      }
    }
    directionOriginals.push([root, "lang", root.getAttribute("lang")]);
    root.setAttribute("lang", target);
  }

  function apply(results, target) {
    setDirection(target);
    for (let { i, r } of results) {
      const block = pending.get(i);
      pending.delete(i);
      if (!block) {
        continue;
      }
      if (block.title) {
        if (document.title === block.sent[0]) {
          document.title = r[0];
        }
        continue;
      }
      if (block.element) {
        // Only if the page hasn't changed it meanwhile.
        if (block.element.getAttribute(block.attribute) === block.sent[0]) {
          attributeOriginals.push([block.element, block.attribute, block.sent[0]]);
          block.element.setAttribute(block.attribute, r[0]);
        }
        continue;
      }
      // A run that stays as it is (translate="no") may have been given words of the sentence
      // around it ("The name" + Deutsche Bahn): hand those to its neighbours.
      r = [...r];
      block.nodes.forEach((node, index) => {
        if (!keep.has(node) || r[index] === undefined) {
          return;
        }
        const own = node.data.trim();
        const at = own ? r[index].indexOf(own) : -1;
        const before = at >= 0 ? r[index].slice(0, at) : "";
        const after = at >= 0 ? r[index].slice(at + own.length) : "";
        r[index] = node.data;
        const previous = index - 1;
        const next = index + 1;
        if (before.trim()) {
          if (previous >= 0 && !keep.has(block.nodes[previous])) r[previous] = (r[previous] || "") + before;
          else if (next < r.length) r[next] = before + (r[next] || "");
        }
        if (after.trim()) {
          if (next < r.length && !keep.has(block.nodes[next])) r[next] = after + (r[next] || "");
          else if (previous >= 0) r[previous] = (r[previous] || "") + after;
        }
      });
      block.nodes.forEach((node, index) => {
        const text = r[index];
        if (text === undefined || node.data !== block.sent[index] || keep.has(node)) {
          return;
        }
        if (!originals.has(node)) {
          originals.set(node, node.data);
        }
        // Keep the run's surrounding whitespace, which separates it from its neighbours.
        const lead = /^\s*/.exec(node.data)[0];
        const trail = /\s*$/.exec(node.data)[0];
        let value = text;
        if (lead && !/^\s/.test(value)) value = lead + value;
        if (trail && !/\s$/.test(value) && value.trim()) value = value + trail;
        translatedText.set(node, value);
        node.data = value;
      });
    }
    // Our own changes aren't news.
    observer.takeRecords();
    return pending.size;
  }

  function restore() {
    observer.disconnect();
    observer.active = false;
    for (const [node, text] of originals) {
      if (translatedText.get(node) === node.data) {
        node.data = text;
      }
    }
    for (const [element, attribute, value] of attributeOriginals) {
      element.setAttribute(attribute, value);
    }
    for (const [element, attribute, value] of directionOriginals) {
      if (value === null) element.removeAttribute(attribute);
      else element.setAttribute(attribute, value);
    }
    if (titleOriginal !== null) {
      document.title = titleOriginal;
    }
    delete window.__stillwaterTranslate;
    return true;
  }

  window.__stillwaterTranslate = { collect, apply, restore };
})();
