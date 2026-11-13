// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// The developer inspector's agent in the inspected page (see inspector.rs). The shell evaluates
// this once per page, while the inspector is open on it, replacing __KEY__ with a random name;
// the agent lives under that one non-enumerable property of `window` and nothing else. Every
// request is `agent.call(method, params)` and answers a JSON string. `destroy` removes the
// highlight overlay, the picker's listeners, the MutationObserver and the property itself.
(function () {
  "use strict";
  var KEY = "__KEY__";
  if (Object.getOwnPropertyDescriptor(window, KEY)) return;

  var stringify = JSON.stringify;
  var instance = Math.random().toString(36).slice(2);
  var ids = new WeakMap();
  var nodes = new Map();
  var nextId = 1;
  var rules = new Map();
  var ruleIds = new WeakMap();
  var nextRule = 1;
  // Declarations switched off in the Styles pane: target key -> [{name, value, important}].
  var disabled = new Map();
  var MAX_CHILDREN = 300;

  function idOf(node) {
    var id = ids.get(node);
    if (!id) {
      id = nextId++;
      ids.set(node, id);
      nodes.set(id, node);
    }
    return id;
  }
  function nodeOf(id) {
    var node = nodes.get(id);
    if (!node) throw new Error("That node is gone (the page changed). Refresh the tree.");
    return node;
  }
  function ruleIdOf(rule) {
    var id = ruleIds.get(rule);
    if (!id) {
      id = nextRule++;
      ruleIds.set(rule, id);
      rules.set(id, rule);
    }
    return id;
  }

  // ---------------------------------------------------------------- the overlay
  var host = null;
  var shadow = null;
  var boxes = null;
  var highlighted = null;

  function ensureOverlay() {
    if (host && host.isConnected) return;
    host = document.createElement("stillwater-inspector-overlay");
    host.style.cssText =
      "all:initial!important;position:fixed!important;left:0!important;top:0!important;" +
      "width:0!important;height:0!important;overflow:visible!important;" +
      "pointer-events:none!important;z-index:2147483647!important;display:block!important";
    try {
      shadow = host.attachShadow({ mode: "closed" });
    } catch (e) {
      shadow = host;
    }
    var style = document.createElement("style");
    style.textContent =
      ".b{position:fixed;box-sizing:border-box;pointer-events:none;border-style:solid;border-width:0}" +
      ".m{border-color:rgba(246,178,107,.66)}.r{border-color:rgba(255,229,153,.66)}" +
      ".p{border-color:rgba(147,196,125,.55)}.c{background:rgba(111,168,220,.66)}" +
      ".l{position:fixed;pointer-events:none;font:11px/1.5 Consolas,monospace;background:#222;" +
      "color:#eee;padding:1px 6px;border-radius:3px;white-space:nowrap;box-shadow:0 1px 4px rgba(0,0,0,.4)}" +
      ".l b{color:#e9a0f0;font-weight:normal}.l i{color:#f5c26b;font-style:normal}.l s{color:#9cdcfe;text-decoration:none}" +
      ".l u{color:#aaa;text-decoration:none}";
    shadow.appendChild(style);
    boxes = {};
    ["m", "r", "p", "c", "l"].forEach(function (name) {
      var element = document.createElement("div");
      element.className = name === "l" ? "l" : "b " + name;
      element.style.display = "none";
      shadow.appendChild(element);
      boxes[name] = element;
    });
    (document.documentElement || document).appendChild(host);
  }

  function px(value) {
    return parseFloat(value) || 0;
  }

  function place(element, left, top, width, height, widths) {
    var style = element.style;
    style.display = "block";
    style.left = left + "px";
    style.top = top + "px";
    style.width = Math.max(0, width) + "px";
    style.height = Math.max(0, height) + "px";
    if (widths) {
      style.borderTopWidth = Math.max(0, widths[0]) + "px";
      style.borderRightWidth = Math.max(0, widths[1]) + "px";
      style.borderBottomWidth = Math.max(0, widths[2]) + "px";
      style.borderLeftWidth = Math.max(0, widths[3]) + "px";
    }
  }

  function hideOverlay() {
    if (!boxes) return;
    for (var name in boxes) boxes[name].style.display = "none";
  }

  function escapeHtml(text) {
    return String(text).replace(/[&<>"]/g, function (c) {
      return { "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" }[c];
    });
  }

  function drawHighlight() {
    var element = highlighted;
    if (!element || !element.isConnected) {
      hideOverlay();
      return;
    }
    if (element.nodeType === 3) {
      var range = document.createRange();
      range.selectNodeContents(element);
      var textRect = range.getBoundingClientRect();
      ensureOverlay();
      hideOverlay();
      place(boxes.c, textRect.left, textRect.top, textRect.width, textRect.height);
      return;
    }
    if (element.nodeType !== 1) {
      hideOverlay();
      return;
    }
    ensureOverlay();
    var rect = element.getBoundingClientRect();
    var cs = getComputedStyle(element);
    var m = [px(cs.marginTop), px(cs.marginRight), px(cs.marginBottom), px(cs.marginLeft)];
    var b = [px(cs.borderTopWidth), px(cs.borderRightWidth), px(cs.borderBottomWidth), px(cs.borderLeftWidth)];
    var p = [px(cs.paddingTop), px(cs.paddingRight), px(cs.paddingBottom), px(cs.paddingLeft)];
    if (cs.display === "inline") {
      m[0] = m[2] = 0;
    }
    place(boxes.m, rect.left - m[3], rect.top - m[0], rect.width + m[1] + m[3], rect.height + m[0] + m[2], m);
    place(boxes.r, rect.left, rect.top, rect.width, rect.height, b);
    place(boxes.p, rect.left + b[3], rect.top + b[0], rect.width - b[1] - b[3], rect.height - b[0] - b[2], p);
    place(
      boxes.c,
      rect.left + b[3] + p[3],
      rect.top + b[0] + p[0],
      rect.width - b[1] - b[3] - p[1] - p[3],
      rect.height - b[0] - b[2] - p[0] - p[2]
    );
    var label = "<b>" + escapeHtml(element.localName) + "</b>";
    if (element.id) label += "<i>#" + escapeHtml(element.id) + "</i>";
    var classes = typeof element.className === "string" ? element.className.trim() : "";
    if (classes) label += "<s>." + escapeHtml(classes.split(/\s+/).slice(0, 3).join(".")) + "</s>";
    label += " <u>" + Math.round(rect.width * 100) / 100 + " × " + Math.round(rect.height * 100) / 100 + "</u>";
    boxes.l.innerHTML = label;
    var top = rect.top - m[0] - 22;
    if (top < 2) top = Math.min(rect.bottom + m[2] + 4, innerHeight - 22);
    place(boxes.l, Math.max(2, Math.min(rect.left, innerWidth - 300)), Math.max(2, top), 0, 0);
    boxes.l.style.width = "auto";
    boxes.l.style.height = "auto";
  }

  var redrawQueued = false;
  function queueRedraw() {
    if (redrawQueued) return;
    redrawQueued = true;
    requestAnimationFrame(function () {
      redrawQueued = false;
      drawHighlight();
    });
  }
  var listening = false;
  function listenForLayout(on) {
    if (on === listening) return;
    listening = on;
    var method = on ? "addEventListener" : "removeEventListener";
    window[method]("scroll", queueRedraw, true);
    window[method]("resize", queueRedraw, true);
  }

  function highlight(node) {
    highlighted = node;
    listenForLayout(!!node);
    drawHighlight();
  }

  // ---------------------------------------------------------------- picking
  var picking = false;
  var picked = null;
  function pickTarget(event) {
    var target = event.target;
    if (target === host) return null;
    // Retarget into open shadow roots.
    var path = event.composedPath ? event.composedPath() : [];
    if (path.length && path[0].nodeType === 1 && path[0] !== host) target = path[0];
    return target && target.nodeType === 1 ? target : null;
  }
  function onPickMove(event) {
    event.preventDefault();
    event.stopPropagation();
    var target = pickTarget(event);
    if (target && target !== highlighted) highlight(target);
  }
  function swallow(event) {
    event.preventDefault();
    event.stopPropagation();
    event.stopImmediatePropagation();
  }
  function onPickClick(event) {
    swallow(event);
    var target = pickTarget(event);
    stopPicking();
    if (target) {
      highlight(target);
      picked = target;
    }
  }
  function onPickKey(event) {
    if (event.key === "Escape") {
      swallow(event);
      stopPicking();
      picked = false;
      highlight(selected);
    }
  }
  var PICK_EVENTS = [
    ["mousemove", onPickMove],
    ["pointermove", onPickMove],
    ["click", onPickClick],
    ["mousedown", swallow],
    ["mouseup", swallow],
    ["pointerdown", swallow],
    ["pointerup", swallow],
    ["auxclick", swallow],
    ["dblclick", swallow],
    ["contextmenu", swallow],
    ["keydown", onPickKey],
  ];
  function startPicking() {
    if (picking) return;
    picking = true;
    PICK_EVENTS.forEach(function (pair) {
      window.addEventListener(pair[0], pair[1], true);
    });
  }
  function stopPicking() {
    if (!picking) return;
    picking = false;
    PICK_EVENTS.forEach(function (pair) {
      window.removeEventListener(pair[0], pair[1], true);
    });
  }

  // ---------------------------------------------------------------- mutations
  var changedChildren = new Set();
  var changedAttributes = new Set();
  var changedText = new Set();
  var observer = new MutationObserver(function (records) {
    for (var i = 0; i < records.length; i++) {
      var record = records[i];
      var target = record.target;
      if (target === host) continue;
      var id = ids.get(target);
      if (record.type === "childList") {
        var onlyOverlay = true;
        var lists = [record.addedNodes, record.removedNodes];
        for (var l = 0; l < 2 && onlyOverlay; l++) {
          for (var n = 0; n < lists[l].length; n++) {
            if (lists[l][n] !== host) {
              onlyOverlay = false;
              break;
            }
          }
        }
        if (onlyOverlay) continue;
        if (id) changedChildren.add(id);
      } else if (record.type === "attributes") {
        if (id) changedAttributes.add(id);
      } else if (id) {
        changedText.add(id);
      }
    }
    if (highlighted) queueRedraw();
  });
  observer.observe(document, { childList: true, subtree: true, attributes: true, characterData: true });

  // ---------------------------------------------------------------- describing nodes
  function isBlankText(node) {
    return node.nodeType === 3 && !/\S/.test(node.data);
  }
  function visibleChildren(node) {
    var list = [];
    if (node.nodeType === 1 && node.shadowRoot) list.push(node.shadowRoot);
    var children = node.nodeType === 1 && node.localName === "template" && node.content ? node.content.childNodes : node.childNodes;
    for (var i = 0; i < children.length; i++) {
      var child = children[i];
      if (child === host || isBlankText(child)) continue;
      list.push(child);
    }
    return list;
  }
  function describe(node) {
    var d = { id: idOf(node), type: node.nodeType };
    switch (node.nodeType) {
      case 1: {
        d.name = node.localName;
        d.attrs = [];
        for (var i = 0; i < node.attributes.length; i++) {
          var attr = node.attributes[i];
          d.attrs.push([attr.name, attr.value.length > 2000 ? attr.value.slice(0, 2000) + "…" : attr.value]);
        }
        var kids = visibleChildren(node);
        d.count = kids.length;
        if (kids.length === 1 && kids[0].nodeType === 3 && kids[0].data.length <= 80 && !/\n/.test(kids[0].data.trim())) {
          d.inline = kids[0].data.trim();
          d.inlineId = idOf(kids[0]);
        }
        break;
      }
      case 3:
      case 8:
        d.text = node.data.length > 5000 ? node.data.slice(0, 5000) + "…" : node.data;
        break;
      case 10:
        d.name = node.name;
        break;
      case 11:
        d.name = node.host ? "#shadow-root (" + node.mode + ")" : "#document-fragment";
        d.count = visibleChildren(node).length;
        break;
      case 9:
        d.name = "#document";
        d.count = visibleChildren(node).length;
        break;
    }
    return d;
  }
  function childrenOf(node, offset) {
    var kids = visibleChildren(node);
    offset = offset || 0;
    return {
      id: idOf(node),
      offset: offset,
      total: kids.length,
      children: kids.slice(offset, offset + MAX_CHILDREN).map(describe),
    };
  }
  function parentOf(node) {
    if (node.parentNode) return node.parentNode;
    if (node.host) return node.host;
    return null;
  }
  // Ancestors of `node` (document first), each with its children, so the tree can open them.
  function pathTo(node) {
    var chain = [];
    for (var at = parentOf(node); at; at = parentOf(at)) chain.unshift(at);
    return { id: idOf(node), chain: chain.map(function (ancestor) { return childrenOf(ancestor, 0); }) };
  }

  // ---------------------------------------------------------------- styles
  function splitTopLevel(text, separator) {
    var parts = [];
    var depth = 0;
    var quote = null;
    var start = 0;
    for (var i = 0; i < text.length; i++) {
      var c = text[i];
      if (quote) {
        if (c === "\\") i++;
        else if (c === quote) quote = null;
      } else if (c === '"' || c === "'") quote = c;
      else if (c === "(" || c === "[") depth++;
      else if (c === ")" || c === "]") depth--;
      else if (c === separator && depth === 0) {
        parts.push(text.slice(start, i));
        start = i + 1;
      }
    }
    parts.push(text.slice(start));
    return parts;
  }
  function declarations(style) {
    var list = [];
    splitTopLevel(style.cssText || "", ";").forEach(function (part) {
      var colon = part.indexOf(":");
      if (colon < 0) return;
      var name = part.slice(0, colon).trim();
      var value = part.slice(colon + 1).trim();
      if (!name) return;
      var important = /!\s*important\s*$/i.test(value);
      if (important) value = value.replace(/\s*!\s*important\s*$/i, "");
      list.push({ name: name, value: value, important: important });
    });
    return list;
  }
  function specificity(selector) {
    // Approximate (a, b, c): ids; classes, attributes and pseudo-classes; types and
    // pseudo-elements. :not()/:is()/:has() count their argument, :where() counts nothing.
    var s = selector.replace(/:where\((?:[^()]|\([^()]*\))*\)/g, "")
      .replace(/"(?:[^"\\]|\\.)*"|'(?:[^'\\]|\\.)*'/g, "")
      .replace(/:(?:not|is|has)\(/g, " (");
    var a = (s.match(/#[\w-]+/g) || []).length;
    var b = (s.match(/\.[\w-]+|\[[^\]]*\]|:(?!:)[\w-]+/g) || []).length;
    var c = (s.replace(/#[\w-]+|\.[\w-]+|\[[^\]]*\]|:(?!:)[\w-]+(\([^)]*\))?/g, " ").match(/(^|[\s>+~(,])[a-zA-Z][\w-]*|::[\w-]+/g) || []).length;
    return a * 1e6 + b * 1e3 + c;
  }
  function sheetLabel(sheet, inlineIndex) {
    if (sheet.href) return sheet.href;
    return "<style> #" + inlineIndex;
  }
  function collectRules(element, sheet, label, out, blocked, media, order) {
    var list;
    try {
      list = sheet.cssRules;
    } catch (e) {
      blocked.push(sheet.href || label);
      return;
    }
    if (!list) return;
    for (var i = 0; i < list.length; i++) {
      var rule = list[i];
      if (rule.selectorText !== undefined && rule.style) {
        var best = -1;
        var matched = [];
        splitTopLevel(rule.selectorText, ",").forEach(function (part) {
          part = part.trim();
          var ok = false;
          try {
            ok = element.matches(part);
          } catch (e) {}
          if (ok) {
            matched.push(part);
            best = Math.max(best, specificity(part));
          }
        });
        if (matched.length) {
          out.push({ rule: rule, selector: rule.selectorText, matched: matched, specificity: best, order: order.n++, source: label, media: media });
        }
      } else if (rule.styleSheet) {
        // @import
        var imported = rule.styleSheet;
        if (!rule.media || !rule.media.mediaText || matchMedia(rule.media.mediaText).matches) {
          collectRules(element, imported, imported.href || label, out, blocked, media, order);
        }
      } else if (rule.cssRules) {
        // @media, @supports, @layer, @container blocks.
        var condition = rule.media ? rule.media.mediaText : rule.conditionText;
        if (rule.media && rule.media.mediaText && !matchMedia(rule.media.mediaText).matches) continue;
        if (rule.conditionText && !rule.media && typeof CSS !== "undefined" && CSS.supports && rule.constructor && /Supports/.test(rule.constructor.name)) {
          try {
            if (!CSS.supports(rule.conditionText)) continue;
          } catch (e) {}
        }
        var at = condition ? "@" + (rule.media ? "media " : "") + condition : media;
        collectRules(element, rule, label, out, blocked, at || media, order);
      }
    }
  }
  function targetKey(target) {
    return target.rule ? "r" + target.rule : "n" + target.node;
  }
  function withDisabled(key, list) {
    var off = disabled.get(key) || [];
    return list.concat(off.map(function (d) {
      return { name: d.name, value: d.value, important: d.important, disabled: true };
    }));
  }
  function stylesOf(id) {
    var element = nodeOf(id);
    if (element.nodeType !== 1) return { notElement: true };
    var found = [];
    var blocked = [];
    var order = { n: 0 };
    var sheets = document.styleSheets;
    var inlineIndex = 0;
    for (var i = 0; i < sheets.length; i++) {
      var sheet = sheets[i];
      if (!sheet.href) inlineIndex++;
      if (sheet.disabled) continue;
      var media = sheet.media && sheet.media.mediaText;
      if (media && !matchMedia(media).matches) continue;
      collectRules(element, sheet, sheetLabel(sheet, inlineIndex), found, blocked, media ? "@media " + media : "", order);
    }
    if (document.adoptedStyleSheets) {
      for (var a = 0; a < document.adoptedStyleSheets.length; a++) {
        collectRules(element, document.adoptedStyleSheets[a], "adopted sheet #" + (a + 1), found, blocked, "", order);
      }
    }
    found.sort(function (x, y) {
      return y.specificity - x.specificity || y.order - x.order;
    });
    var result = {
      id: id,
      inline: element.style ? withDisabled("n" + id, declarations(element.style)) : null,
      rules: found.map(function (entry) {
        var ruleId = ruleIdOf(entry.rule);
        return {
          rule: ruleId,
          selector: entry.selector,
          matched: entry.matched,
          source: entry.source,
          media: entry.media,
          decls: withDisabled("r" + ruleId, declarations(entry.rule.style)),
        };
      }),
      blocked: blocked,
      box: boxModel(element),
    };
    return result;
  }
  function boxModel(element) {
    var cs = getComputedStyle(element);
    var rect = element.getBoundingClientRect();
    var side = function (prefix, suffix) {
      return ["Top", "Right", "Bottom", "Left"].map(function (s) {
        return Math.round(px(cs[prefix + s + (suffix || "")]) * 100) / 100;
      });
    };
    var border = side("border", "Width");
    var padding = side("padding");
    return {
      margin: side("margin"),
      border: border,
      padding: padding,
      width: Math.round((rect.width - border[1] - border[3] - padding[1] - padding[3]) * 100) / 100,
      height: Math.round((rect.height - border[0] - border[2] - padding[0] - padding[2]) * 100) / 100,
      position: cs.position,
    };
  }
  function styleFor(target) {
    if (target.rule) {
      var rule = rules.get(target.rule);
      if (!rule) throw new Error("That rule is gone.");
      return rule.style;
    }
    return nodeOf(target.node).style;
  }
  // Set, rename, remove (empty value), switch off or back on a declaration.
  function setDeclaration(params) {
    var style = styleFor(params.target);
    var key = targetKey(params.target);
    var off = disabled.get(key) || [];
    var name = (params.name || "").trim();
    var value = (params.value || "").trim();
    var important = /!\s*important\s*$/i.test(value) || !!params.important;
    value = value.replace(/\s*!\s*important\s*$/i, "");
    var old = params.oldName;
    // A re-enabled or edited declaration that was off is no longer off.
    off = off.filter(function (d) {
      return d.name !== (old || name);
    });
    if (old && old !== name) style.removeProperty(old);
    if (params.disable) {
      off.push({ name: name, value: value, important: important });
      style.removeProperty(name);
    } else if (!name) {
      // Nothing to add.
    } else if (!value) {
      style.removeProperty(name);
    } else {
      style.setProperty(name, value, important ? "important" : "");
      if (!style.getPropertyValue(name)) {
        disabled.set(key, off);
        return { invalid: true, name: name, value: value };
      }
    }
    disabled.set(key, off);
    return { ok: true };
  }
  function computedOf(id) {
    var element = nodeOf(id);
    if (element.nodeType !== 1) return [];
    var cs = getComputedStyle(element);
    var list = [];
    for (var i = 0; i < cs.length; i++) {
      var name = cs[i];
      list.push([name, cs.getPropertyValue(name)]);
    }
    if (!list.length) {
      for (var prop in cs) {
        if (/^[a-z]/.test(prop) && typeof cs[prop] === "string" && isNaN(prop)) {
          list.push([prop.replace(/[A-Z]/g, function (c) { return "-" + c.toLowerCase(); }), cs[prop]]);
        }
      }
    }
    list.sort(function (a, b) {
      return a[0] < b[0] ? -1 : a[0] > b[0] ? 1 : 0;
    });
    return list;
  }

  // ---------------------------------------------------------------- console values
  function nodeLabel(node) {
    if (node.nodeType === 1) {
      var text = node.localName;
      if (node.id) text += "#" + node.id;
      if (typeof node.className === "string" && node.className.trim()) text += "." + node.className.trim().split(/\s+/).join(".");
      return text;
    }
    if (node.nodeType === 3) return '#text "' + node.data.slice(0, 60) + '"';
    return node.nodeName;
  }
  function className(value) {
    try {
      var tag = Object.prototype.toString.call(value).slice(8, -1);
      var ctor = value.constructor && value.constructor.name;
      return ctor || tag;
    } catch (e) {
      return "Object";
    }
  }
  function preview(value, depth) {
    var type = typeof value;
    if (value === null) return { t: "null" };
    if (type === "undefined") return { t: "undefined" };
    if (type === "number" || type === "boolean") return { t: type, v: String(value) };
    if (type === "bigint") return { t: "number", v: value + "n" };
    if (type === "string") return { t: "string", v: value.length > 10000 ? value.slice(0, 10000) + "…" : value };
    if (type === "symbol") return { t: "symbol", v: value.toString() };
    if (type === "function") return { t: "function", v: "ƒ " + (value.name || "anonymous") + "()" };
    try {
      if (typeof Node !== "undefined" && value instanceof Node) {
        return { t: "node", id: idOf(value), v: nodeLabel(value) };
      }
      if (value instanceof Error) {
        return { t: "error", v: value.name + ": " + value.message, stack: String(value.stack || "") };
      }
    } catch (e) {}
    var name = className(value);
    var isArray = Array.isArray(value);
    var result = { t: isArray ? "array" : "object", c: name };
    var keys;
    try {
      keys = isArray ? null : Object.keys(value);
    } catch (e) {
      keys = [];
    }
    var size = isArray ? value.length : keys.length;
    result.n = size;
    if (depth >= 2) return result;
    var entries = [];
    var limit = 100;
    if (isArray) {
      for (var i = 0; i < Math.min(size, limit); i++) {
        var item;
        try {
          item = preview(value[i], depth + 1);
        } catch (e) {
          item = { t: "error", v: String(e) };
        }
        entries.push([String(i), item]);
      }
    } else {
      for (var k = 0; k < Math.min(keys.length, limit); k++) {
        var entry;
        try {
          entry = preview(value[keys[k]], depth + 1);
        } catch (e) {
          entry = { t: "error", v: String(e) };
        }
        entries.push([keys[k], entry]);
      }
    }
    result.e = entries;
    return result;
  }
  function evaluate(code) {
    var value;
    try {
      if (/\$0/.test(code)) {
        var wrapped = (0, eval)("(function($0){return eval(arguments[1]);})");
        value = wrapped(selected, code);
      } else {
        value = (0, eval)(code);
      }
    } catch (error) {
      if (error && error.name === "EvalError" && /eval|content security/i.test(error.message || "")) {
        return { blocked: true, message: String(error.message) };
      }
      return { thrown: preview(error, 0) };
    }
    return { value: preview(value, 0) };
  }

  // ---------------------------------------------------------------- network and sources
  function networkEntries() {
    var list = [];
    try {
      var entries = performance.getEntriesByType("navigation").concat(performance.getEntriesByType("resource"));
      entries.forEach(function (entry) {
        list.push({
          url: entry.name,
          type: entry.initiatorType || "navigation",
          start: Math.round(entry.startTime),
          duration: Math.round(entry.duration),
          size: entry.transferSize || 0,
          body: entry.decodedBodySize || entry.encodedBodySize || 0,
          status: entry.responseStatus || 0,
        });
      });
    } catch (e) {}
    return list;
  }
  var sourceCache = new Map();
  function sourceList() {
    var list = [];
    var scripts = document.scripts;
    for (var i = 0; i < scripts.length; i++) {
      var script = scripts[i];
      list.push({ kind: "script", index: i, url: script.src || "", inline: !script.src, size: script.src ? 0 : script.text.length });
    }
    var sheets = document.styleSheets;
    for (var s = 0; s < sheets.length; s++) {
      var owner = sheets[s].ownerNode;
      list.push({ kind: "style", index: s, url: sheets[s].href || "", inline: !sheets[s].href, size: !sheets[s].href && owner ? owner.textContent.length : 0 });
    }
    return list;
  }
  function sourceText(params) {
    var url;
    if (params.kind === "script") {
      var script = document.scripts[params.index];
      if (!script) return { error: "That script is gone." };
      if (!script.src) return { text: script.text };
      url = script.src;
    } else {
      var sheet = document.styleSheets[params.index];
      if (!sheet) return { error: "That stylesheet is gone." };
      if (!sheet.href) return { text: sheet.ownerNode ? sheet.ownerNode.textContent : "" };
      url = sheet.href;
    }
    var cached = sourceCache.get(url);
    if (cached) return cached;
    sourceCache.set(url, { pending: true });
    try {
      fetch(url, { credentials: "same-origin" })
        .then(function (response) {
          if (!response.ok) throw new Error("HTTP " + response.status);
          return response.text();
        })
        .then(function (text) {
          sourceCache.set(url, { text: text, url: url });
        }, function (error) {
          // A script from another site without CORS can't be read by the page either.
          var rules = "";
          if (params.kind === "style") {
            try {
              var sheet = document.styleSheets[params.index];
              rules = Array.prototype.map.call(sheet.cssRules, function (rule) { return rule.cssText; }).join("\n");
            } catch (e) {}
          }
          sourceCache.set(url, rules ? { text: rules, url: url, note: "Rebuilt from the parsed rules" } : { error: "Couldn't read " + url + " (" + error.message + "; another site's file that doesn't allow reading)." });
        });
    } catch (e) {
      sourceCache.set(url, { error: String(e) });
    }
    return { pending: true };
  }

  // ---------------------------------------------------------------- requests
  var selected = null;
  function replaceOuterHTML(node, html) {
    var parent = node.parentNode;
    if (!parent) throw new Error("This node can't be replaced.");
    if (node.nodeType !== 1) {
      var text = document.createTextNode(html);
      parent.replaceChild(text, node);
      return text;
    }
    var before = node.previousSibling;
    var template = document.createElement("template");
    template.innerHTML = html;
    var fragment = template.content;
    var first = fragment.firstChild;
    while (first && first.nodeType === 3 && !/\S/.test(first.data)) first = first.nextSibling;
    if (parent.nodeType === 9 || node === document.documentElement || node === document.body || node === document.head) {
      // outerHTML on these needs care: replace children and attributes instead.
      var parsed = new DOMParser().parseFromString(node === document.documentElement ? html : "<!doctype html><html><" + node.localName + ">" + "</" + node.localName + ">", "text/html");
      if (node === document.documentElement) {
        document.documentElement.innerHTML = parsed.documentElement.innerHTML;
        return document.documentElement;
      }
      var match = /^\s*<[^>]*>([\s\S]*)<\/[^>]*>\s*$/.exec(html);
      node.innerHTML = match ? match[1] : html;
      return node;
    }
    parent.replaceChild(fragment, node);
    var result = before ? before.nextSibling : parent.firstChild;
    while (result && result.nodeType === 3 && !/\S/.test(result.data)) result = result.nextSibling;
    return result || parent;
  }
  function setAttributes(params) {
    var element = nodeOf(params.id);
    if (params.remove && element.hasAttribute(params.remove)) element.removeAttribute(params.remove);
    (params.attrs || []).forEach(function (pair) {
      element.setAttribute(pair[0], pair[1]);
    });
    return describe(element);
  }
  var methods = {
    root: function () {
      return { url: location.href, title: document.title, doc: describe(document), children: childrenOf(document, 0) };
    },
    children: function (p) {
      return childrenOf(nodeOf(p.id), p.offset || 0);
    },
    describe: function (p) {
      return describe(nodeOf(p.id));
    },
    select: function (p) {
      selected = p.id ? nodeOf(p.id) : null;
      if (selected && selected.nodeType === 1 && p.scroll && selected.scrollIntoView) {
        var rect = selected.getBoundingClientRect();
        if (rect.bottom < 0 || rect.top > innerHeight || rect.right < 0 || rect.left > innerWidth) {
          selected.scrollIntoView({ block: "center", inline: "nearest" });
        }
      }
      highlight(selected);
      return true;
    },
    highlight: function (p) {
      highlight(p.id ? nodeOf(p.id) : selected);
      return true;
    },
    reveal: function (p) {
      return pathTo(nodeOf(p.id));
    },
    pick: function (p) {
      if (p.stop) {
        stopPicking();
        highlight(selected);
      } else startPicking();
      return picking;
    },
    inspectAt: function (p) {
      var element = document.elementFromPoint(p.x, p.y);
      if (!element || element === host) return null;
      selected = element;
      highlight(element);
      return pathTo(element);
    },
    poll: function () {
      var result = {
        children: Array.from(changedChildren),
        attrs: Array.from(changedAttributes),
        text: Array.from(changedText),
        picking: picking,
      };
      changedChildren.clear();
      changedAttributes.clear();
      changedText.clear();
      if (picked) {
        selected = picked;
        result.picked = pathTo(picked);
      } else if (picked === false) {
        result.pickCancelled = true;
      }
      picked = null;
      return result;
    },
    setAttributes: setAttributes,
    setText: function (p) {
      var node = nodeOf(p.id);
      if (node.nodeType === 1) node.textContent = p.text;
      else node.data = p.text;
      return describe(node);
    },
    outerHTML: function (p) {
      var node = nodeOf(p.id);
      return node.nodeType === 1 ? node.outerHTML : node.nodeType === 3 ? node.data : node.nodeType === 8 ? "<!--" + node.data + "-->" : "";
    },
    setOuterHTML: function (p) {
      var replaced = replaceOuterHTML(nodeOf(p.id), p.html);
      selected = replaced;
      highlight(replaced);
      return pathTo(replaced);
    },
    remove: function (p) {
      var node = nodeOf(p.id);
      var parent = parentOf(node);
      if (!node.parentNode || node === document.documentElement) throw new Error("This node can't be deleted.");
      var next = node.nextSibling || node.previousSibling;
      while (next && isBlankText(next)) next = next.nextSibling;
      node.parentNode.removeChild(node);
      if (selected === node) selected = null;
      highlight(selected);
      return { parent: parent ? idOf(parent) : 0, next: next ? idOf(next) : 0 };
    },
    styles: function (p) {
      return stylesOf(p.id);
    },
    computed: function (p) {
      return computedOf(p.id);
    },
    setDeclaration: setDeclaration,
    eval: function (p) {
      return evaluate(p.code);
    },
    expand: function (p) {
      // A deeper look at a value from an earlier evaluation: an element's properties.
      return { value: preview(nodeOf(p.id), 1) };
    },
    network: networkEntries,
    sources: sourceList,
    sourceText: sourceText,
    destroy: function () {
      stopPicking();
      listenForLayout(false);
      observer.disconnect();
      if (host && host.parentNode) host.parentNode.removeChild(host);
      host = shadow = boxes = highlighted = selected = picked = null;
      nodes.clear();
      rules.clear();
      disabled.clear();
      sourceCache.clear();
      delete window[KEY];
      return true;
    },
  };

  var agent = {
    call: function (method, params) {
      var answer;
      try {
        var fn = Object.prototype.hasOwnProperty.call(methods, method) ? methods[method] : null;
        if (!fn) throw new Error("Unknown request " + method);
        answer = { inst: instance, r: fn(params || {}) };
      } catch (error) {
        answer = { inst: instance, error: String((error && error.message) || error) };
      }
      try {
        return stringify(answer);
      } catch (error) {
        return stringify({ inst: instance, error: "Couldn't describe the answer: " + error });
      }
    },
  };
  Object.defineProperty(window, KEY, { value: agent, configurable: true, enumerable: false, writable: false });
})()
