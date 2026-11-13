// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// The developer inspector panel (F12). It talks only to the shell (see inspector.rs):
//   send(cmd, data): console.log("__inspector__" + JSON)
//   call(method, params): a request to the agent in the inspected page; the shell answers with
//     window.inspector.reply(id, json).
//   window.inspector.push(message): target changes, console messages, pick requests.
"use strict";

const $ = (id) => document.getElementById(id);
const PREFIX = "__inspector__";
const send = (cmd, data) => console.log(PREFIX + JSON.stringify(Object.assign({ cmd }, data || {})));

// ---------------------------------------------------------------- bridge
let seq = 0;
const pending = new Map();
let instance = null;

function call(method, params) {
  return new Promise((resolve, reject) => {
    const id = ++seq;
    pending.set(id, { resolve, reject, method });
    send("call", { id, method, params: params || {} });
  });
}

function reply(id, text) {
  const waiter = pending.get(id);
  pending.delete(id);
  if (!waiter) return;
  let answer;
  try {
    answer = JSON.parse(text);
  } catch (e) {
    waiter.reject(new Error("Bad answer from the page"));
    return;
  }
  if (answer.inst) {
    if (waiter.method !== "root" && instance && answer.inst !== instance) {
      // The page navigated or reloaded: its nodes are new.
      scheduleReload();
    }
    instance = answer.inst;
  }
  if (answer.error) waiter.reject(new Error(answer.error));
  else waiter.resolve(answer.r);
}

function toast(text) {
  const element = document.createElement("div");
  element.className = "toast";
  element.textContent = text;
  document.body.appendChild(element);
  setTimeout(() => element.remove(), 3500);
}

function fail(error) {
  toast(String((error && error.message) || error));
}

// ---------------------------------------------------------------- state
const nodes = new Map(); // id -> description, plus { kids: [ids] | null, total, open }
const elementsOf = new Map(); // id -> the tree's DOM element for that node
let selected = 0;
let rootId = 0;
let currentTab = "elements";
let reloadTimer = 0;

// The tree is (re)loaded: "scheduled", "loading" or "ready". Requests that show a node in it
// (pick, Inspect from the page's menu) wait for "ready".
let treeState = "scheduled";
let treeWaiters = [];
function treeReady() {
  return treeState === "ready" ? Promise.resolve() : new Promise((resolve) => treeWaiters.push(resolve));
}
function scheduleReload() {
  clearTimeout(reloadTimer);
  treeState = "scheduled";
  reloadTimer = setTimeout(loadTree, 150);
}

// ---------------------------------------------------------------- tree rendering
function esc(text) {
  return String(text).replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" })[c]);
}

function remember(description) {
  const old = nodes.get(description.id);
  const node = Object.assign(old || { kids: null, total: 0, open: false }, description);
  if (old && old.count !== description.count && !old.open) node.kids = null;
  nodes.set(description.id, node);
  return node;
}

function setChildren(answer) {
  const parent = nodes.get(answer.id) || remember({ id: answer.id, type: 1, name: "?" });
  const list = answer.offset ? (parent.kids || []).slice(0, answer.offset) : [];
  for (const child of answer.children) list.push(remember(child).id);
  parent.kids = list;
  parent.total = answer.total;
  return parent;
}

function attrsHtml(node) {
  return node.attrs
    .map(
      ([name, value], index) =>
        ` <span class="attr" data-index="${index}"><span class="an">${esc(name)}</span>` +
        (value === "" ? "" : `=<span class="br">"</span><span class="av">${esc(value)}</span><span class="br">"</span>`) +
        "</span>"
    )
    .join("");
}

function lineHtml(node) {
  const expandable = (node.type === 1 || node.type === 9 || node.type === 11) && node.count > 0 && node.inline === undefined;
  const twisty = `<span class="tw ${expandable ? (node.open ? "o" : "c") : ""}"></span>`;
  switch (node.type) {
    case 1: {
      const open = `<span class="br">&lt;</span><span class="tn">${esc(node.name)}</span>${attrsHtml(node)}<span class="br">&gt;</span>`;
      const close = `<span class="br">&lt;/</span><span class="tn">${esc(node.name)}</span><span class="br">&gt;</span>`;
      const empty = /^(area|base|br|col|embed|hr|img|input|link|meta|source|track|wbr)$/.test(node.name);
      if (node.inline !== undefined) return twisty + open + `<span class="tx inl">${esc(node.inline)}</span>` + close;
      if (!node.count) return twisty + open + (empty ? "" : close);
      return twisty + open + (node.open ? "" : `<span class="dim">…</span>${close}`);
    }
    case 3:
      return twisty + `<span class="tx">"${esc(node.text)}"</span>`;
    case 8:
      return twisty + `<span class="cm">&lt;!--${esc(node.text)}--&gt;</span>`;
    case 10:
      return twisty + `<span class="cm">&lt;!DOCTYPE ${esc(node.name)}&gt;</span>`;
    default:
      return twisty + `<span class="dim">${esc(node.name)}</span>`;
  }
}

function renderNode(id, depth) {
  const node = nodes.get(id);
  const wrap = document.createElement("div");
  wrap.className = "n";
  wrap.dataset.id = id;
  const line = document.createElement("div");
  line.className = "line" + (id === selected ? " sel" : "");
  line.style.paddingLeft = depth * 14 + 2 + "px";
  line.innerHTML = lineHtml(node);
  wrap.appendChild(line);
  if (node.open && node.kids) {
    const kids = document.createElement("div");
    kids.className = "kids";
    for (const kid of node.kids) kids.appendChild(renderNode(kid, depth + 1));
    if (node.kids.length < node.total) {
      const more = document.createElement("div");
      more.className = "line more";
      more.style.paddingLeft = (depth + 1) * 14 + 14 + "px";
      more.dataset.more = id;
      more.textContent = `Show ${Math.min(300, node.total - node.kids.length)} more of ${node.total - node.kids.length}…`;
      kids.appendChild(more);
    }
    wrap.appendChild(kids);
    if (node.type === 1) {
      const close = document.createElement("div");
      close.className = "line close";
      close.style.paddingLeft = depth * 14 + 14 + "px";
      close.innerHTML = `<span class="br">&lt;/</span><span class="tn">${esc(node.name)}</span><span class="br">&gt;</span>`;
      wrap.appendChild(close);
    }
  }
  wrap.depth = depth;
  elementsOf.set(id, wrap);
  return wrap;
}

function rerender(id) {
  const old = elementsOf.get(id);
  if (!old || !old.isConnected) return;
  const fresh = renderNode(id, old.depth);
  old.replaceWith(fresh);
}

async function loadTree() {
  treeState = "loading";
  try {
    await loadTreeNow();
  } finally {
    if (treeState === "loading") {
      treeState = "ready";
      for (const resolve of treeWaiters.splice(0)) resolve();
    }
  }
}
async function loadTreeNow() {
  nodes.clear();
  elementsOf.clear();
  const previous = selectedPath();
  selected = 0;
  const tree = $("tree");
  try {
    const answer = await call("root");
    rootId = answer.doc.id;
    const root = remember(answer.doc);
    setChildren(answer.children);
    root.open = true;
    tree.textContent = "";
    for (const kid of root.kids) tree.appendChild(renderNode(kid, 0));
    // Open <html> and <body> like other browsers do.
    const html = root.kids.map((id) => nodes.get(id)).find((node) => node.type === 1);
    if (html) {
      await expand(html.id, true);
      const body = (html.kids || []).map((id) => nodes.get(id)).find((node) => node.name === "body");
      if (body) await expand(body.id, true);
    }
    updateButtons();
    showStyles();
    if (previous) restoreSelection(previous);
  } catch (error) {
    tree.innerHTML = `<p class="pad dim">${esc(error.message)}</p>`;
  }
}

// A path of (name, index among siblings) from the root, to find the same node after a reload.
function selectedPath() {
  if (!selected) return null;
  const path = [];
  let id = selected;
  for (;;) {
    const parent = [...nodes.values()].find((node) => node.kids && node.kids.includes(id));
    if (!parent) break;
    path.unshift(parent.kids.indexOf(id));
    id = parent.id;
  }
  return path;
}

async function restoreSelection(path) {
  let id = rootId;
  for (const index of path) {
    const node = nodes.get(id);
    if (!node.kids) await expand(id, true);
    const next = nodes.get(id).kids && nodes.get(id).kids[index];
    if (!next) return;
    if (index !== path[path.length - 1] || id !== rootId) await expand(id, true);
    id = next;
  }
  if (id !== rootId) select(id, { scroll: true, page: false });
}

async function expand(id, open) {
  const node = nodes.get(id);
  if (!node) return;
  if (open && !node.kids) {
    try {
      setChildren(await call("children", { id }));
    } catch (error) {
      fail(error);
      return;
    }
  }
  node.open = open;
  if (id === rootId) return;
  rerender(id);
}

async function showMore(id) {
  const node = nodes.get(id);
  try {
    setChildren(await call("children", { id, offset: node.kids.length }));
    rerender(id);
  } catch (error) {
    fail(error);
  }
}

// Open the tree down to a node the agent described with `pathTo` and select it.
async function reveal(path) {
  if (!path) return;
  await treeReady();
  for (const level of path.chain) {
    if (!nodes.has(level.id)) remember({ id: level.id, type: 1, name: "?" });
    setChildren(level);
    nodes.get(level.id).open = true;
  }
  // Re-render from the highest ancestor already on screen.
  const tree = $("tree");
  if (!tree.childElementCount || !elementsOf.size) {
    tree.textContent = "";
    for (const kid of nodes.get(rootId).kids || []) tree.appendChild(renderNode(kid, 0));
  } else {
    const shown = path.chain.find((level) => elementsOf.get(level.id) && elementsOf.get(level.id).isConnected);
    if (shown) rerender(shown.id);
    else {
      tree.textContent = "";
      for (const kid of nodes.get(rootId).kids || []) tree.appendChild(renderNode(kid, 0));
    }
  }
  select(path.id, { scroll: true, page: false });
  const element = elementsOf.get(path.id);
  if (element) {
    element.firstChild.classList.add("flash");
    setTimeout(() => element.firstChild && element.firstChild.classList.remove("flash"), 900);
  }
}

// ---------------------------------------------------------------- selection
function select(id, options) {
  options = options || {};
  const old = elementsOf.get(selected);
  if (old && old.firstChild) old.firstChild.classList.remove("sel");
  selected = id;
  const element = elementsOf.get(id);
  if (element && element.firstChild) {
    element.firstChild.classList.add("sel");
    if (options.scroll) {
      const line = element.firstChild;
      const tree = $("tree");
      const top = line.offsetTop - tree.offsetTop;
      if (top < tree.scrollTop || top > tree.scrollTop + tree.clientHeight - 20) tree.scrollTop = top - tree.clientHeight / 3;
    }
  }
  if (options.page !== false) call("select", { id, scroll: true }).catch(() => {});
  else call("select", { id }).catch(() => {});
  updateButtons();
  updateCrumbs();
  showStyles();
}

function parentOf(id) {
  for (const node of nodes.values()) if (node.kids && node.kids.includes(id)) return node.id;
  return 0;
}

function updateCrumbs() {
  const crumbs = $("crumbs");
  crumbs.textContent = "";
  const chain = [];
  for (let id = selected; id && id !== rootId; id = parentOf(id)) chain.unshift(id);
  for (const id of chain) {
    const node = nodes.get(id);
    if (!node || node.type !== 1) continue;
    const span = document.createElement("span");
    let label = node.name;
    const attrs = new Map(node.attrs);
    if (attrs.get("id")) label += "#" + attrs.get("id");
    else if (attrs.get("class")) label += "." + attrs.get("class").trim().split(/\s+/).slice(0, 2).join(".");
    span.textContent = label;
    span.dataset.id = id;
    if (crumbs.childElementCount) crumbs.appendChild(document.createTextNode(" › "));
    crumbs.appendChild(span);
  }
}

function updateButtons() {
  const node = nodes.get(selected);
  const element = node && node.type === 1;
  $("edit-html").disabled = !node || !(element || node.type === 3 || node.type === 8);
  $("add-attr").disabled = !element;
  $("delete-node").disabled = !node || node.name === "html";
}

// ---------------------------------------------------------------- tree events
const tree = $("tree");
tree.addEventListener("click", (event) => {
  const more = event.target.closest("[data-more]");
  if (more) {
    showMore(+more.dataset.more);
    return;
  }
  const wrap = event.target.closest(".n");
  if (!wrap) return;
  const id = +wrap.dataset.id;
  if (event.target.classList.contains("tw")) {
    expand(id, !nodes.get(id).open);
    return;
  }
  if (!event.target.closest(".line.close") || true) select(id);
});
tree.addEventListener("dblclick", (event) => {
  const wrap = event.target.closest(".n");
  if (!wrap || event.target.closest(".edit")) return;
  const id = +wrap.dataset.id;
  const node = nodes.get(id);
  const attr = event.target.closest(".attr");
  if (attr) {
    editAttribute(id, +attr.dataset.index, attr);
  } else if (event.target.classList.contains("tx") && node.type === 3) {
    editText(id, event.target);
  } else if (event.target.classList.contains("inl")) {
    editText(node.inlineId, event.target);
  } else if (event.target.classList.contains("tn") && node.type === 1) {
    editHtml(id);
  } else if (node.type === 1 && node.count && node.inline === undefined) {
    expand(id, !node.open);
  }
});
let hoverTimer = 0;
let hovered = 0;
tree.addEventListener("mousemove", (event) => {
  const wrap = event.target.closest(".n");
  const id = wrap ? +wrap.dataset.id : 0;
  if (id === hovered) return;
  hovered = id;
  clearTimeout(hoverTimer);
  hoverTimer = setTimeout(() => call("highlight", { id: hovered || selected }).catch(() => {}), 40);
});
tree.addEventListener("mouseleave", () => {
  hovered = 0;
  clearTimeout(hoverTimer);
  call("highlight", { id: selected }).catch(() => {});
});
tree.addEventListener("keydown", (event) => {
  if (event.target !== tree) return;
  const lines = [...tree.querySelectorAll(".n")].filter((n) => n.offsetParent !== null);
  const index = lines.findIndex((n) => +n.dataset.id === selected);
  const node = nodes.get(selected);
  switch (event.key) {
    case "ArrowDown":
      if (index + 1 < lines.length) select(+lines[index + 1].dataset.id, { scroll: true });
      break;
    case "ArrowUp":
      if (index > 0) select(+lines[index - 1].dataset.id, { scroll: true });
      break;
    case "ArrowRight":
      if (node && node.count && node.inline === undefined && !node.open) expand(selected, true);
      else if (index + 1 < lines.length) select(+lines[index + 1].dataset.id, { scroll: true });
      break;
    case "ArrowLeft":
      if (node && node.open) expand(selected, false);
      else if (parentOf(selected) && parentOf(selected) !== rootId) select(parentOf(selected), { scroll: true });
      break;
    case "Delete":
      deleteNode();
      break;
    case "F2":
      editHtml(selected);
      break;
    case "Enter":
      if (node && node.type === 1) addAttribute();
      break;
    default:
      return;
  }
  event.preventDefault();
});
$("crumbs").addEventListener("click", (event) => {
  if (event.target.dataset.id) select(+event.target.dataset.id, { scroll: true });
});

// An inline text editor over `target`; resolves with the new text or null when cancelled.
function inlineEdit(target, text, wide) {
  return new Promise((resolve) => {
    const input = document.createElement("input");
    input.className = "edit";
    input.value = text;
    input.spellcheck = false;
    input.style.width = Math.max(80, Math.min(wide ? 600 : 400, text.length * 7.5 + 30)) + "px";
    const placeholder = target;
    placeholder.replaceWith(input);
    input.focus();
    input.select();
    let done = false;
    const finish = (value) => {
      if (done) return;
      done = true;
      if (input.isConnected) input.replaceWith(placeholder);
      resolve(value);
      tree.focus();
    };
    input.addEventListener("keydown", (event) => {
      event.stopPropagation();
      if (event.key === "Enter") finish(input.value);
      else if (event.key === "Escape") finish(null);
    });
    input.addEventListener("blur", () => finish(input.value));
  });
}

// Parse `name="value" other=x` into pairs, the way the page's parser would.
function parseAttributes(text) {
  const template = document.createElement("template");
  template.innerHTML = `<div ${text}></div>`;
  const element = template.content.firstElementChild;
  if (!element) return [];
  return [...element.attributes].map((attr) => [attr.name, attr.value]);
}

async function editAttribute(id, index, target) {
  const node = nodes.get(id);
  const [name, value] = node.attrs[index];
  const text = await inlineEdit(target, value === "" ? name : `${name}="${value.replace(/"/g, "&quot;")}"`, true);
  if (text === null) return;
  try {
    remember(await call("setAttributes", { id, remove: name, attrs: parseAttributes(text) }));
  } catch (error) {
    fail(error);
  }
  rerender(id);
  showStyles();
}

async function addAttribute() {
  const id = selected;
  const element = elementsOf.get(id);
  if (!element) return;
  const anchor = element.firstChild.querySelector(".br:last-of-type") || element.firstChild.lastChild;
  const spot = document.createElement("span");
  anchor.before(spot);
  const text = await inlineEdit(spot, "", true);
  spot.remove();
  if (!text) return;
  try {
    remember(await call("setAttributes", { id, attrs: parseAttributes(text) }));
  } catch (error) {
    fail(error);
  }
  rerender(id);
  showStyles();
}

async function editText(id, target) {
  const node = nodes.get(id);
  const text = await inlineEdit(target, node ? node.text || "" : target.textContent, true);
  if (text === null) return;
  try {
    remember(await call("setText", { id, text }));
  } catch (error) {
    fail(error);
  }
  const parent = parentOf(id);
  if (parent) {
    try {
      remember(await call("describe", { id: parent }));
    } catch (e) {}
  }
  rerender(parent && parent !== rootId ? parent : id);
}

let editingHtml = 0;
async function editHtml(id) {
  if (!id) return;
  try {
    const html = await call("outerHTML", { id });
    editingHtml = id;
    $("html-text").value = html;
    $("html-editor").hidden = false;
    $("html-text").focus();
  } catch (error) {
    fail(error);
  }
}
async function applyHtml() {
  const id = editingHtml;
  $("html-editor").hidden = true;
  editingHtml = 0;
  if (!id) return;
  const parent = parentOf(id);
  try {
    const path = await call("setOuterHTML", { id, html: $("html-text").value });
    if (parent && nodes.get(parent)) {
      nodes.get(parent).kids = null;
      nodes.get(parent).open = false;
    }
    await reveal(path);
  } catch (error) {
    fail(error);
  }
}
$("html-apply").addEventListener("click", applyHtml);
$("html-cancel").addEventListener("click", () => {
  $("html-editor").hidden = true;
  editingHtml = 0;
});
$("html-text").addEventListener("keydown", (event) => {
  if (event.key === "Enter" && event.ctrlKey) {
    event.preventDefault();
    applyHtml();
  } else if (event.key === "Escape") {
    $("html-editor").hidden = true;
    editingHtml = 0;
  }
});

async function deleteNode() {
  const id = selected;
  const node = nodes.get(id);
  if (!node || node.name === "html") return;
  const parent = parentOf(id);
  try {
    const answer = await call("remove", { id });
    nodes.delete(id);
    if (parent && nodes.get(parent)) {
      const p = nodes.get(parent);
      p.kids = p.kids.filter((kid) => kid !== id);
      p.total = Math.max(0, p.total - 1);
      p.count = Math.max(0, (p.count || 1) - 1);
      if (parent === rootId) {
        const element = elementsOf.get(id);
        if (element) element.remove();
      } else rerender(parent);
    }
    const next = answer.next && nodes.has(answer.next) ? answer.next : parent !== rootId ? parent : 0;
    if (next) select(next, { page: false });
    else selected = 0;
    updateButtons();
  } catch (error) {
    fail(error);
  }
}

$("edit-html").addEventListener("click", () => editHtml(selected));
$("add-attr").addEventListener("click", addAttribute);
$("delete-node").addEventListener("click", deleteNode);
$("refresh-tree").addEventListener("click", loadTree);

// ---------------------------------------------------------------- styles
let stylesToken = 0;
let subTab = "styles";

async function showStyles() {
  const token = ++stylesToken;
  const node = nodes.get(selected);
  const pane = $("styles");
  if (!node || node.type !== 1) {
    pane.innerHTML = `<p class="dim pad">Select an element to see its styles.</p>`;
    $("computed-list").textContent = "";
    return;
  }
  if (subTab === "computed") {
    try {
      const list = await call("computed", { id: selected });
      if (token === stylesToken) renderComputed(list);
    } catch (error) {
      $("computed-list").textContent = error.message;
    }
    return;
  }
  try {
    const styles = await call("styles", { id: selected });
    if (token === stylesToken) renderStyles(styles);
  } catch (error) {
    pane.innerHTML = `<p class="dim pad">${esc(error.message)}</p>`;
  }
}

const COLOR = /^(#[0-9a-f]{3,8}|rgba?\([^)]*\)|hsla?\([^)]*\)|[a-z]+)$/i;
function valueHtml(value) {
  const swatch = /(#[0-9a-f]{3,8}\b|rgba?\([^)]*\)|hsla?\([^)]*\))/i.exec(value);
  const color = swatch ? swatch[1] : COLOR.test(value) && CSS.supports && CSS.supports("color", value) ? value : null;
  return (color ? `<span class="swatch" style="background:${esc(color)}"></span>` : "") + esc(value);
}

function declHtml(decl, index) {
  return (
    `<div class="decl${decl.disabled ? " off" : ""}" data-index="${index}">` +
    `<input type="checkbox" ${decl.disabled ? "" : "checked"} title="Turn this declaration on or off">` +
    `<span class="pn">${esc(decl.name)}</span>:&nbsp;<span class="pv">${valueHtml(decl.value)}</span>` +
    (decl.important ? `<span class="imp">&nbsp;!important</span>` : "") +
    ";</div>"
  );
}

let lastStyles = null;
function renderStyles(styles) {
  lastStyles = styles;
  const pane = $("styles");
  let html = "";
  if (styles.inline) {
    html +=
      `<div class="rule" data-target="inline"><div class="head"><span class="sel-text">element.style {</span>` +
      `<button class="add" title="Add a declaration">+</button></div>` +
      styles.inline.map(declHtml).join("") +
      "}</div>";
  }
  styles.rules.forEach((rule, index) => {
    const selector = rule.selector
      .split(",")
      .map((part) => (rule.matched.includes(part.trim()) ? esc(part.trim()) : `<span class="no">${esc(part.trim())}</span>`))
      .join(", ");
    html +=
      `<div class="rule" data-target="${index}">` +
      (rule.media ? `<div class="media">${esc(rule.media)}</div>` : "") +
      `<div class="head"><span class="sel-text">${selector} {</span><span class="src" title="${esc(rule.source)}">${esc(shortSource(rule.source))}</span>` +
      `<button class="add" title="Add a declaration">+</button></div>` +
      rule.decls.map(declHtml).join("") +
      "}</div>";
  });
  for (const sheet of styles.blocked) {
    html += `<div class="blocked">Rules from ${esc(sheet)} can't be shown: the stylesheet comes from another site that doesn't allow reading it (CORS).</div>`;
  }
  if (!styles.rules.length && !(styles.inline && styles.inline.length)) html += `<p class="dim pad">No matching rules from the page's stylesheets.</p>`;
  html += boxHtml(styles.box);
  pane.innerHTML = html;
}

function shortSource(source) {
  try {
    const url = new URL(source);
    return (url.pathname.split("/").pop() || url.host) + (url.search ? "?" : "");
  } catch (e) {
    return source;
  }
}

function boxHtml(box) {
  if (!box) return "";
  const v = (n) => (n === 0 ? "–" : String(n));
  const ring = (cls, label, sides, inner) =>
    `<div class="bm ${cls}"><span class="lbl">${label}</span><span class="t">${v(sides[0])}</span><span class="r">${v(sides[1])}</span>` +
    `<span class="b">${v(sides[2])}</span><span class="l">${v(sides[3])}</span>${inner}</div>`;
  const content = `<div class="bm content">${box.width} × ${box.height}</div>`;
  return (
    `<div class="boxmodel">` +
    ring("margin", "margin", box.margin, ring("border", "border", box.border, ring("padding", "padding", box.padding, content))) +
    `</div>` +
    (box.position && box.position !== "static" ? `<p class="dim pad small">position: ${esc(box.position)}</p>` : "")
  );
}

function targetOf(ruleElement) {
  const which = ruleElement.dataset.target;
  if (which === "inline") return { node: lastStyles.id };
  return { rule: lastStyles.rules[+which].rule };
}
function declsOf(ruleElement) {
  const which = ruleElement.dataset.target;
  return which === "inline" ? lastStyles.inline : lastStyles.rules[+which].decls;
}

async function setDeclaration(params) {
  try {
    const answer = await call("setDeclaration", params);
    if (answer && answer.invalid) toast(`"${answer.name}: ${answer.value}" isn't a valid declaration.`);
  } catch (error) {
    fail(error);
  }
  showStyles();
  call("highlight", { id: selected }).catch(() => {});
}

$("styles").addEventListener("change", (event) => {
  if (event.target.type !== "checkbox") return;
  const declElement = event.target.closest(".decl");
  const ruleElement = event.target.closest(".rule");
  const decl = declsOf(ruleElement)[+declElement.dataset.index];
  setDeclaration({ target: targetOf(ruleElement), name: decl.name, value: decl.value, important: decl.important, disable: !event.target.checked });
});

$("styles").addEventListener("click", async (event) => {
  const ruleElement = event.target.closest(".rule");
  if (!ruleElement) return;
  if (event.target.classList.contains("add")) {
    addDeclaration(ruleElement);
    return;
  }
  const field = event.target.closest(".pn, .pv");
  if (!field || event.target.closest(".edit")) return;
  const declElement = field.closest(".decl");
  const decl = declsOf(ruleElement)[+declElement.dataset.index];
  const isName = field.classList.contains("pn");
  const text = await inlineEdit(field, isName ? decl.name : decl.value + (decl.important ? " !important" : ""));
  if (text === null) return;
  if (isName) {
    if (text.trim() === decl.name) return;
    setDeclaration({ target: targetOf(ruleElement), oldName: decl.name, name: text.trim(), value: text.trim() ? decl.value : "", important: decl.important });
  } else {
    if (text.trim() === decl.value + (decl.important ? " !important" : "")) return;
    setDeclaration({ target: targetOf(ruleElement), oldName: decl.disabled ? decl.name : undefined, name: decl.name, value: text });
  }
});

async function addDeclaration(ruleElement) {
  const row = document.createElement("div");
  row.className = "decl";
  const spot = document.createElement("span");
  row.appendChild(spot);
  const closing = ruleElement.lastChild;
  ruleElement.insertBefore(row, closing && closing.nodeType === 3 ? closing : null);
  const text = await inlineEdit(spot, "");
  row.remove();
  if (!text || !text.trim()) return;
  // "name: value" in one go, or just a name (then ask for the value).
  let name = text;
  let value = "";
  const colon = text.indexOf(":");
  if (colon > 0) {
    name = text.slice(0, colon);
    value = text.slice(colon + 1).replace(/;\s*$/, "");
  } else {
    const row2 = document.createElement("div");
    row2.className = "decl";
    row2.innerHTML = `<span class="pn">${esc(name.trim())}</span>:&nbsp;`;
    const spot2 = document.createElement("span");
    row2.appendChild(spot2);
    ruleElement.insertBefore(row2, closing && closing.nodeType === 3 ? closing : null);
    value = await inlineEdit(spot2, "");
    row2.remove();
    if (!value) return;
  }
  setDeclaration({ target: targetOf(ruleElement), name: name.trim(), value });
}

function renderComputed(list) {
  const filter = $("computed-filter").value.trim().toLowerCase();
  const html = list
    .filter(([name, value]) => !filter || name.includes(filter) || value.toLowerCase().includes(filter))
    .map(([name, value]) => `<div><span class="pn">${esc(name)}:</span><span class="pv">${valueHtml(value)}</span></div>`)
    .join("");
  $("computed-list").innerHTML = html || `<p class="dim">No properties.</p>`;
  lastComputed = list;
}
let lastComputed = [];
$("computed-filter").addEventListener("input", () => renderComputed(lastComputed));

document.querySelectorAll(".subtabs button").forEach((button) =>
  button.addEventListener("click", () => {
    subTab = button.dataset.sub;
    document.querySelectorAll(".subtabs button").forEach((b) => b.classList.toggle("on", b === button));
    document.querySelectorAll(".sub").forEach((sub) => sub.classList.toggle("on", sub.id === subTab));
    showStyles();
  })
);

// ---------------------------------------------------------------- console
const log = $("log");
let errors = 0;
let lastMessage = null;

function addMessage(level, content) {
  const atBottom = log.scrollTop + log.clientHeight >= log.scrollHeight - 4;
  if (typeof content === "string" && lastMessage && lastMessage.level === level && lastMessage.text === content) {
    lastMessage.count++;
    let badge = lastMessage.element.querySelector(".count");
    if (!badge) {
      badge = document.createElement("span");
      badge.className = "count";
      lastMessage.element.prepend(badge);
    }
    badge.textContent = lastMessage.count;
    return;
  }
  const element = document.createElement("div");
  element.className = "msg " + level;
  if (typeof content === "string") element.textContent = content;
  else element.appendChild(content);
  log.appendChild(element);
  while (log.childElementCount > 2000) log.firstChild.remove();
  lastMessage = typeof content === "string" ? { level, text: content, count: 1, element } : null;
  applyConsoleFilter(element);
  if (atBottom) log.scrollTop = log.scrollHeight;
  if (level === "error" && currentTab !== "console") {
    errors++;
    $("errcount").textContent = errors;
    $("errcount").hidden = false;
  }
}

function applyConsoleFilter(element) {
  const filter = $("console-filter").value.toLowerCase();
  const level = ["error", "warn"].find((l) => element.classList.contains(l)) || "log";
  const shown =
    (level === "error" ? $("show-errors").checked : level === "warn" ? $("show-warnings").checked : $("show-logs").checked) &&
    (!filter || element.textContent.toLowerCase().includes(filter));
  element.hidden = !shown;
}
["console-filter", "show-errors", "show-warnings", "show-logs"].forEach((id) =>
  $(id).addEventListener("input", () => log.querySelectorAll(".msg").forEach(applyConsoleFilter))
);
$("console-clear").addEventListener("click", () => {
  log.textContent = "";
  lastMessage = null;
});

function valueElement(value, top) {
  const span = document.createElement("span");
  switch (value.t) {
    case "string":
      span.className = "v-string";
      span.textContent = top ? JSON.stringify(value.v).length > 200 ? value.v : JSON.stringify(value.v) : JSON.stringify(value.v);
      return span;
    case "number":
    case "boolean":
    case "null":
    case "undefined":
    case "symbol":
    case "function":
      span.className = "v-" + value.t;
      span.textContent = value.v !== undefined ? value.v : value.t;
      return span;
    case "node":
      span.className = "v-node";
      span.textContent = value.v;
      span.title = "Show in Elements";
      span.addEventListener("click", async () => {
        try {
          showTab("elements");
          await reveal(await call("reveal", { id: value.id }));
        } catch (error) {
          fail(error);
        }
      });
      span.addEventListener("mouseenter", () => call("highlight", { id: value.id }).catch(() => {}));
      span.addEventListener("mouseleave", () => call("highlight", { id: selected }).catch(() => {}));
      return span;
    case "error":
      span.textContent = value.v + (value.stack ? "\n" + value.stack : "");
      return span;
    default: {
      const label = value.t === "array" ? `${value.c || "Array"}(${value.n})` : value.c && value.c !== "Object" ? value.c : "{…}";
      if (!value.e) {
        span.className = "v-null";
        span.textContent = label;
        return span;
      }
      const details = document.createElement("details");
      details.className = "v-obj";
      const summary = document.createElement("summary");
      const short = value.e
        .slice(0, 5)
        .map(([key, item]) => (value.t === "array" ? "" : key + ": ") + shortValue(item))
        .join(", ");
      summary.textContent = (value.t === "array" ? `(${value.n}) [` : `${value.c && value.c !== "Object" ? value.c + " " : ""}{`) + short + (value.n > 5 ? ", …" : "") + (value.t === "array" ? "]" : "}");
      details.appendChild(summary);
      const body = document.createElement("div");
      for (const [key, item] of value.e) {
        const row = document.createElement("div");
        const name = document.createElement("span");
        name.className = "v-key";
        name.textContent = key + ": ";
        row.appendChild(name);
        row.appendChild(valueElement(item, false));
        body.appendChild(row);
      }
      if (value.n > value.e.length) {
        const more = document.createElement("div");
        more.className = "dim";
        more.textContent = `… ${value.n - value.e.length} more`;
        body.appendChild(more);
      }
      details.appendChild(body);
      return details;
    }
  }
}
function shortValue(item) {
  if (item.t === "string") return JSON.stringify(item.v.length > 30 ? item.v.slice(0, 30) + "…" : item.v);
  if (item.t === "array") return `Array(${item.n})`;
  if (item.t === "object") return item.c && item.c !== "Object" ? item.c : "{…}";
  if (item.t === "node") return item.v;
  return item.v !== undefined ? item.v : item.t;
}

const inputHistory = [];
let historyAt = 0;
const input = $("input");
async function run(code) {
  inputHistory.push(code);
  historyAt = inputHistory.length;
  addMessage("input", code);
  let answer;
  try {
    answer = await call("eval", { code });
  } catch (error) {
    addMessage("error", error.message);
    return;
  }
  if (answer.blocked) {
    // The page's Content-Security-Policy forbids eval: let the shell run the code directly.
    send("raw-eval", { code });
    return;
  }
  if (answer.thrown) {
    const element = valueElement(answer.thrown, true);
    addMessage("error", "Uncaught " + element.textContent);
    return;
  }
  addMessage("result", valueElement(answer.value, true));
}
input.addEventListener("keydown", (event) => {
  if (event.key === "Enter" && !event.shiftKey) {
    event.preventDefault();
    const code = input.value;
    if (!code.trim()) return;
    input.value = "";
    input.rows = 1;
    run(code);
  } else if (event.key === "ArrowUp" && !input.value.includes("\n") && historyAt > 0) {
    event.preventDefault();
    input.value = inputHistory[--historyAt];
  } else if (event.key === "ArrowDown" && !input.value.includes("\n") && historyAt < inputHistory.length) {
    event.preventDefault();
    historyAt++;
    input.value = inputHistory[historyAt] || "";
  }
});
input.addEventListener("input", () => {
  input.rows = Math.min(10, input.value.split("\n").length);
});

// ---------------------------------------------------------------- network
let networkRows = [];
function size(bytes) {
  if (!bytes) return "–";
  if (bytes < 1024) return bytes + " B";
  if (bytes < 1024 * 1024) return (bytes / 1024).toFixed(1) + " kB";
  return (bytes / 1024 / 1024).toFixed(1) + " MB";
}
async function loadNetwork() {
  try {
    networkRows = await call("network");
  } catch (error) {
    networkRows = [];
    fail(error);
  }
  renderNetwork();
}
function renderNetwork() {
  const filter = $("network-filter").value.toLowerCase();
  const rows = networkRows.filter((row) => !filter || row.url.toLowerCase().includes(filter));
  const body = $("network-table").tBodies[0];
  body.innerHTML = rows
    .map((row) => {
      let name = row.url;
      try {
        const url = new URL(row.url);
        name = (url.pathname.split("/").pop() || url.host + url.pathname) + url.search;
      } catch (e) {}
      return (
        `<tr title="${esc(row.url)}"><td>${esc(name)}</td><td class="${row.status >= 400 ? "bad" : ""}">${row.status || "–"}</td>` +
        `<td>${esc(row.type)}</td><td>${size(row.size || row.body)}</td><td>${row.duration} ms</td></tr>`
      );
    })
    .join("");
  const total = rows.reduce((sum, row) => sum + (row.size || row.body || 0), 0);
  $("network-summary").textContent = `${rows.length} requests, ${size(total)}`;
}
$("network-refresh").addEventListener("click", loadNetwork);
$("network-filter").addEventListener("input", renderNetwork);

// ---------------------------------------------------------------- sources
let sources = [];
async function loadSources() {
  try {
    sources = await call("sources");
  } catch (error) {
    sources = [];
    fail(error);
  }
  $("source-list").innerHTML = sources
    .map((source, index) => {
      let name = source.inline ? `(inline ${source.kind === "script" ? "script" : "<style>"} ${source.index + 1})` : source.url;
      try {
        if (!source.inline) name = new URL(source.url).pathname.split("/").pop() || source.url;
      } catch (e) {}
      return `<div data-index="${index}" title="${esc(source.url || name)}"><span class="kind">${source.kind === "script" ? "JS" : "CSS"}</span>${esc(name)}</div>`;
    })
    .join("") || `<p class="dim pad">No scripts or stylesheets.</p>`;
}
$("source-list").addEventListener("click", async (event) => {
  const row = event.target.closest("[data-index]");
  if (!row) return;
  $("source-list").querySelectorAll(".sel").forEach((r) => r.classList.remove("sel"));
  row.classList.add("sel");
  const source = sources[+row.dataset.index];
  const view = $("source-view");
  view.innerHTML = `<p class="dim pad">Loading…</p>`;
  for (let tries = 0; tries < 50; tries++) {
    let answer;
    try {
      answer = await call("sourceText", { kind: source.kind, index: source.index });
    } catch (error) {
      view.innerHTML = `<p class="dim pad">${esc(error.message)}</p>`;
      return;
    }
    if (answer.pending) {
      await new Promise((resolve) => setTimeout(resolve, 200));
      continue;
    }
    if (answer.error) {
      view.innerHTML = `<p class="dim pad">${esc(answer.error)}</p>`;
      return;
    }
    const pre = document.createElement("pre");
    pre.textContent = answer.text;
    view.textContent = "";
    if (answer.note) view.insertAdjacentHTML("beforeend", `<p class="dim pad small">${esc(answer.note)}</p>`);
    view.appendChild(pre);
    return;
  }
  view.innerHTML = `<p class="dim pad">Timed out loading the file.</p>`;
});

// ---------------------------------------------------------------- tabs, toolbar
function showTab(name) {
  currentTab = name;
  document.querySelectorAll("#tabs button").forEach((b) => b.classList.toggle("on", b.dataset.tab === name));
  document.querySelectorAll(".tab").forEach((tab) => tab.classList.toggle("on", tab.id === name));
  if (name === "console") {
    errors = 0;
    $("errcount").hidden = true;
    input.focus();
    log.scrollTop = log.scrollHeight;
  } else if (name === "network") loadNetwork();
  else if (name === "sources") loadSources();
}
document.querySelectorAll("#tabs button").forEach((button) => button.addEventListener("click", () => showTab(button.dataset.tab)));

let picking = false;
function setPicking(on) {
  picking = on;
  $("pick").classList.toggle("on", on);
}
async function togglePick() {
  try {
    setPicking(await call("pick", { stop: picking }));
  } catch (error) {
    fail(error);
  }
}
$("pick").addEventListener("click", togglePick);
let dock = "right";
$("dock").addEventListener("click", () => send("dock", { dock: dock === "right" ? "bottom" : "right" }));
$("close").addEventListener("click", () => send("close"));
document.addEventListener("keydown", (event) => {
  if (event.key === "Escape" && picking) {
    call("pick", { stop: true }).then(setPicking, () => {});
  } else if (event.ctrlKey && event.shiftKey && (event.key === "C" || event.key === "c")) {
    event.preventDefault();
    togglePick();
  }
});

// ---------------------------------------------------------------- polling the page
let polling = false;
async function poll() {
  if (polling || !instance) return;
  polling = true;
  try {
    const changes = await call("poll");
    setPicking(changes.picking);
    if (changes.picked) {
      showTab("elements");
      await reveal(changes.picked);
    }
    const refreshed = new Set();
    for (const id of changes.children) {
      const node = nodes.get(id);
      if (!node) continue;
      if (node.open) {
        try {
          setChildren(await call("children", { id }));
          if (id !== rootId) remember(await call("describe", { id }));
        } catch (e) {
          continue;
        }
      } else {
        node.kids = null;
        try {
          remember(await call("describe", { id }));
        } catch (e) {
          continue;
        }
      }
      refreshed.add(id);
    }
    for (const id of changes.attrs.concat(changes.text)) {
      if (!nodes.has(id) || refreshed.has(id)) continue;
      try {
        remember(await call("describe", { id }));
      } catch (e) {
        continue;
      }
      refreshed.add(id);
    }
    // Re-render changed nodes whose ancestors weren't re-rendered already.
    for (const id of refreshed) {
      if (id === rootId) {
        const tree = $("tree");
        tree.textContent = "";
        for (const kid of nodes.get(rootId).kids || []) tree.appendChild(renderNode(kid, 0));
        break;
      }
      rerender(id);
    }
    // A text change inside an element shown inline updates its parent's line.
    for (const id of changes.text) {
      const parent = parentOf(id);
      if (parent && parent !== rootId && nodes.get(parent).inlineId === id) {
        try {
          remember(await call("describe", { id: parent }));
          rerender(parent);
        } catch (e) {}
      }
    }
    if (selected && (changes.attrs.includes(selected) || changes.children.length)) {
      // Classes or structure changed: matching rules may have too.
      if (changes.attrs.includes(selected)) showStyles();
    }
  } catch (e) {
    // The page may be navigating; the next answer brings a new instance.
  } finally {
    polling = false;
  }
}
setInterval(poll, 500);

// ---------------------------------------------------------------- from the shell
window.inspector = {
  reply,
  push(message) {
    switch (message.type) {
      case "target":
        $("target").textContent = message.title || message.url || "";
        $("target").title = message.url || "";
        if (message.tab) {
          log.textContent = "";
          lastMessage = null;
        }
        instance = null;
        scheduleReload();
        break;
      case "console":
        for (const item of message.items) addMessage(item.level === "info" || item.level === "debug" ? "log" : item.level, item.text);
        break;
      case "raw-result":
        if (message.error) addMessage("error", message.error);
        else addMessage("result", valueElement(message.value, true));
        break;
      case "pick":
        showTab("elements");
        call("pick", { stop: false }).then(setPicking, fail);
        break;
      case "inspectAt":
        showTab("elements");
        call("inspectAt", { x: message.x, y: message.y }).then(reveal, fail);
        break;
      case "dock":
        dock = message.dock;
        $("dock").title = dock === "right" ? "Dock to bottom" : "Dock to right";
        $("dockmark").setAttribute("x", dock === "right" ? "9" : "2");
        $("dockmark").setAttribute("y", dock === "right" ? "3" : "8.5");
        $("dockmark").setAttribute("width", dock === "right" ? "5" : "12");
        $("dockmark").setAttribute("height", dock === "right" ? "10" : "4.5");
        break;
    }
  },
  // For tests (STILLWATER_TEST_INSPECTOR_SCRIPT).
  _: { call, send, nodes, select, expand, reveal, run, showTab, setDeclaration, deleteNode, applyHtml, get selected() { return selected; }, get styles() { return lastStyles; } },
};

send("ready");
