/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

// Talks to servoshell (desktop/sidebar.rs):
//   page -> Rust: console.log("__sidebar__<command>\t<argument>")
//   Rust -> page: window.sidebar.setState({...})

"use strict";

const send = (command, argument = "") => console.log(`__sidebar__${command}\t${argument}`);

const $ = (id) => document.getElementById(id);
const url = $("url");
const urlbar = $("urlbar");
const tabs = $("tabs");
let state = { tabs: [], url: "", canGoBack: false, canGoForward: false, compact: false };

function tabLabel(tab) {
  if (tab.title) return tab.title;
  if (!tab.url || tab.url === "servo:newtab") return "New Tab";
  try {
    return new URL(tab.url).host || tab.url;
  } catch (_) {
    return tab.url;
  }
}

function tabIcon(tab) {
  if (tab.favicon) {
    const img = document.createElement("img");
    img.className = "tab-icon";
    img.src = tab.favicon;
    return img;
  }
  const letter = document.createElement("span");
  letter.className = "tab-icon letter";
  letter.textContent = tabLabel(tab).trim().charAt(0).toUpperCase() || "•";
  return letter;
}

function iconButton(className, glyph, title, onClick) {
  const button = document.createElement("button");
  button.className = `${className} icon`;
  button.textContent = glyph;
  button.title = title;
  button.addEventListener("click", (event) => {
    event.stopPropagation();
    onClick(event);
  });
  return button;
}

// ---- Double-click detection (Servo may not fire dblclick) ----
function onDoubleClick(element, handler) {
  let last = 0;
  element.addEventListener("click", (event) => {
    const now = Date.now();
    if (now - last < 400) {
      last = 0;
      event.stopPropagation();
      handler(event);
    } else {
      last = now;
    }
  });
}

// ---- Inline rename: swap a label for a text field ----
function startRename(label, current, commit) {
  const input = document.createElement("input");
  input.className = "rename-input";
  input.value = current;
  label.replaceWith(input);
  input.focus();
  input.select();
  let done = false;
  const finish = (save) => {
    if (done) return;
    done = true;
    if (save) commit(input.value.trim());
    input.replaceWith(label);
  };
  input.addEventListener("click", (event) => event.stopPropagation());
  input.addEventListener("keydown", (event) => {
    if (event.key === "Enter") finish(true);
    else if (event.key === "Escape") finish(false);
  });
  input.addEventListener("blur", () => finish(true));
}

// ---- Context menu ----
const menu = $("menu");
function closeMenu() {
  menu.hidden = true;
  menu.replaceChildren();
}
function openMenu(items, x, y) {
  menu.replaceChildren(...items.map((item) => {
    if (item === "-") {
      const line = document.createElement("div");
      line.className = "menu-separator";
      return line;
    }
    if (item.note) {
      const note = document.createElement("div");
      note.className = "menu-note";
      note.textContent = item.note;
      return note;
    }
    if (item.header) {
      const header = document.createElement("div");
      header.className = "menu-header";
      const logo = document.createElement("img");
      logo.src = "resource:///stillwater-icon.png";
      logo.alt = "";
      const name = document.createElement("span");
      name.textContent = item.header;
      header.append(logo, name);
      return header;
    }
    const button = document.createElement("button");
    button.className = "menu-item";
    if (item.glyph || item.shortcut) {
      button.classList.add("rich");
      const glyph = document.createElement("span");
      glyph.className = "menu-glyph";
      glyph.textContent = item.glyph || "";
      const label = document.createElement("span");
      label.className = "menu-label";
      label.textContent = item.label;
      const shortcut = document.createElement("span");
      shortcut.className = "menu-shortcut";
      shortcut.textContent = item.shortcut || "";
      button.append(glyph, label, shortcut);
    } else {
      button.textContent = item.label;
    }
    button.addEventListener("click", (event) => {
      event.stopPropagation();
      closeMenu();
      item.run();
    });
    return button;
  }));
  menu.hidden = false;
  const width = menu.offsetWidth;
  const height = menu.offsetHeight;
  menu.style.left = `${Math.max(4, Math.min(x, window.innerWidth - width - 4))}px`;
  menu.style.top = `${Math.max(4, Math.min(y, window.innerHeight - height - 4))}px`;
}
document.addEventListener("mousedown", (event) => {
  if (!menu.hidden && !menu.contains(event.target)) closeMenu();
});
document.addEventListener("keydown", (event) => {
  if (event.key === "Escape" && !menu.hidden) closeMenu();
});
function attachMenu(element, itemsFor) {
  element.addEventListener("contextmenu", (event) => {
    event.preventDefault();
    openMenu(itemsFor(), event.clientX, event.clientY);
  });
}

function otherSpaces() {
  if (!state.workspaces) return [];
  return state.workspaces.list.filter((space) => space.id !== state.workspaces.current);
}

function folderNames() {
  return (state.pinned && state.pinned.folders) || [];
}

// Menu for an ordinary tab.
function tabMenu(tab, label) {
  return [
    { label: "Pin tab", run: () => send("pin", tab.id) },
    { label: "Add to Essentials", run: () => send("essential", tab.id) },
    { label: "Rename", run: () => startRename(label, tabLabel(tab), (title) => send("tab-rename", `${tab.id}\t${title}`)) },
    { label: "Open in split view", run: () => send("split-tab", tab.id) },
    ...otherSpaces().map((space) => ({ label: `Move to ${space.name}`, run: () => send("tab-move-space", `${tab.id}\t${space.id}`) })),
    ...(tab.active && /^https?:/.test(tab.url || "") ? [{ label: "Compare with Edge", run: () => send("compare") }] : []),
    { label: "Report site isn't working", run: () => send("report-site", tab.id) },
    "-",
    ...(tab.loaded && !tab.active ? [{ label: "Unload (free memory)", run: () => send("tab-unload", tab.id) }] : []),
    { label: "Close tab", run: () => send("close", tab.id) },
    ...(state.session && state.session.closed ? [{ label: "Reopen closed tab", run: () => send("session-reopen") }] : []),
  ];
}

// Menu for a pinned tab or Essential.
function pinMenu(pin, label, essential) {
  const items = [];
  if (pin.loaded) items.push({ label: "Unload (free memory)", run: () => send("pin-unload", pin.pinId) });
  if (pin.changed) items.push({ label: "Back to pinned page", run: () => send("pin-reset", pin.pinId) });
  if (label) {
    items.push({ label: "Rename", run: () => startRename(label, pin.title, (title) => send("pin-rename", `${pin.pinId}\t${title}`)) });
  }
  if (!essential) {
    if (items.length) items.push("-");
    for (const folder of folderNames()) {
      if (folder.folderId !== pin.folder) {
        items.push({ label: `Move to folder “${folder.name}”`, run: () => send("pin-folder", `${pin.pinId}\t${folder.folderId}`) });
      }
    }
    if (pin.folder) items.push({ label: "Remove from folder", run: () => send("pin-folder", `${pin.pinId}\t`) });
    items.push({ label: "New folder with this tab", run: () => send("folder-new", pin.pinId) });
    items.push({ label: "Move up", run: () => send("pin-move", `${pin.pinId}\t-1`) });
    items.push({ label: "Move down", run: () => send("pin-move", `${pin.pinId}\t1`) });
  }
  items.push("-", { label: "Open in split view", run: () => send("split-pin", pin.pinId) });
  if (items.length) items.push("-");
  items.push({
    label: essential ? "Remove from Essentials" : "Add to Essentials",
    run: () => send("pin-essential", pin.pinId),
  });
  items.push({ label: "Unpin", run: () => send("unpin", pin.pinId) });
  if (pin.loaded) items.push({ label: "Report site isn't working", run: () => send("report-site", "") });
  return items;
}

// One row for an ordinary tab ({id, …}) or a pinned tab ({pinId, …}).
function renderRow(item, isPin) {
  const row = document.createElement("div");
  row.className = "tab" +
    (item.active ? " selected" : "") +
    (item.loading ? " loading" : "") +
    (item.loaded === false ? " unloaded" : "");
  row.title = item.url;
  // For drag and drop (session.js).
  if (isPin) {
    row.dataset.pin = item.pinId;
    row.dataset.folder = item.folder || "";
  } else {
    row.dataset.tab = item.id;
  }

  const label = document.createElement("span");
  label.className = "tab-label";
  label.textContent = tabLabel(item);
  onDoubleClick(label, () => {
    if (isPin) startRename(label, item.title, (title) => send("pin-rename", `${item.pinId}\t${title}`));
    else startRename(label, tabLabel(item), (title) => send("tab-rename", `${item.id}\t${title}`));
  });

  const items = () => (isPin ? pinMenu(item, label, false) : tabMenu(item, label));
  const more = iconButton("tab-more", "", "More", () => {
    const box = more.getBoundingClientRect();
    openMenu(items(), box.left, box.bottom);
  });
  const parts = [tabIcon(item), label];
  if (item.playing) parts.push(mediaIndicator());
  if (isPin && item.changed) {
    parts.push(iconButton("tab-reset", "", "Back to pinned page", () => send("pin-reset", item.pinId)));
  }
  parts.push(more);
  if (!isPin) {
    parts.push(iconButton("tab-close", "", "Close tab", () => send("close", item.id)));
  } else if (item.loaded) {
    parts.push(iconButton("tab-close", "", "Unload tab", () => send("pin-unload", item.pinId)));
  }
  row.append(...parts);

  row.addEventListener("click", () => (isPin ? send("pin-open", item.pinId) : send("activate", item.id)));
  row.addEventListener("mouseup", (event) => {
    if (event.button !== 1) return; // middle click closes / unloads
    if (isPin) send("pin-unload", item.pinId);
    else send("close", item.id);
  });
  attachMenu(row, items);
  return row;
}

// The page shown in split view, under the tab list.
function renderSplitRow(split) {
  const row = document.createElement("div");
  row.className = "tab split-row" + (state.paneFocused && !state.glance ? " selected" : "");
  row.title = split.url;
  const icon = document.createElement("span");
  icon.className = "tab-icon split-icon";
  const label = document.createElement("span");
  label.className = "tab-label";
  label.textContent = split.title || split.url;
  const close = iconButton("tab-close", "", "Close split view", () => send("split-close", ""));
  row.append(icon, label, close);
  attachMenu(row, () => [{ label: "Close split view", run: () => send("split-close", "") }]);
  return row;
}

// Essentials: square favicon tiles at the top.
function renderEssentials() {
  const essentials = state.essentials || [];
  const grid = $("essentials");
  grid.hidden = essentials.length === 0;
  grid.replaceChildren(...essentials.map((pin) => {
    const tile = document.createElement("button");
    tile.className = "essential" + (pin.active ? " selected" : "") + (pin.loaded ? "" : " unloaded");
    tile.title = pin.title;
    tile.dataset.pin = pin.pinId;
    tile.dataset.essential = "1";
    const icon = tabIcon(pin);
    icon.classList.add("essential-icon");
    tile.append(icon);
    tile.addEventListener("click", () => send("pin-open", pin.pinId));
    tile.addEventListener("mouseup", (event) => {
      if (event.button === 1) send("pin-unload", pin.pinId);
    });
    attachMenu(tile, () => pinMenu(pin, null, true));
    return tile;
  }));
}

// This space's pinned section: folders (collapsible), then loose pins.
function renderPinned() {
  const pinned = state.pinned || { folders: [], pins: [] };
  const rows = [];
  for (const folder of pinned.folders) {
    const header = document.createElement("div");
    header.className = "tab folder" + (folder.collapsed ? " collapsed" : "");
    header.dataset.folderId = folder.folderId;
    const icon = document.createElement("span");
    icon.className = "icon tab-icon folder-icon";
    icon.textContent = folder.collapsed ? "" : "";
    const label = document.createElement("span");
    label.className = "tab-label";
    label.textContent = folder.name;
    const count = document.createElement("span");
    count.className = "folder-count";
    count.textContent = folder.collapsed && folder.pins.length ? String(folder.pins.length) : "";
    header.append(icon, label, count);
    header.addEventListener("click", () => send("folder-toggle", folder.folderId));
    const rename = () => startRename(label, folder.name, (name) => send("folder-rename", `${folder.folderId}\t${name}`));
    onDoubleClick(label, rename);
    attachMenu(header, () => [
      { label: "Rename folder", run: rename },
      { label: folder.collapsed ? "Expand" : "Collapse", run: () => send("folder-toggle", folder.folderId) },
      "-",
      { label: "Delete folder (keeps its tabs)", run: () => send("folder-delete", folder.folderId) },
    ]);
    rows.push(header);
    if (!folder.collapsed) {
      for (const pin of folder.pins) {
        const row = renderRow(pin, true);
        row.classList.add("in-folder");
        rows.push(row);
      }
    }
  }
  for (const pin of pinned.pins) rows.push(renderRow(pin, true));
  $("pinned").replaceChildren(...rows);
  $("pinned-separator").hidden = rows.length === 0;
}

function currentWorkspace() {
  const { list, current } = state.workspaces;
  return list.find((workspace) => workspace.id === current) || list[0];
}

function renderWorkspaces() {
  const workspace = currentWorkspace();
  const root = document.documentElement;
  document.body.style.background = state.background;
  if (workspace.color) root.style.setProperty("--ws-accent", workspace.color);
  else root.style.removeProperty("--ws-accent");

  $("ws-icon").textContent = workspace.icon;
  $("ws-name").textContent = workspace.name;
  $("ws-delete").disabled = state.workspaces.list.length < 2;
  // Spaces off (the default): one list of tabs, no switcher and no space controls.
  const spaces = state.workspaces.enabled !== false;
  document.body.classList.toggle("no-spaces", !spaces);

  const strip = state.workspaces.list.map((space, index) => {
    const button = document.createElement("button");
    button.className = "workspace" + (space.id === workspace.id ? " active" : "");
    button.title = `${space.name} (Ctrl+Alt+${index + 1})`;
    button.dataset.space = space.id;
    button.textContent = space.icon;
    button.addEventListener("click", () => send("ws-switch", space.id));
    return button;
  });
  const add = document.createElement("button");
  add.className = "workspace add";
  add.title = "New space";
  add.textContent = "";
  add.addEventListener("click", () => send("ws-new"));
  $("workspaces").replaceChildren(...strip, add);
  root.style.setProperty("--sidebar-bg", state.background);
}

function renderUpdate() {
  const button = $("update");
  const update = state.update;
  button.hidden = !update;
  if (!update) {
    document.getElementById("update-error").hidden = true;
    return;
  }
  const status = update.status;
  button.disabled = status === "downloading" || status === "installing";
  button.classList.toggle("failed", status.startsWith("failed"));
  const error = document.getElementById("update-error");
  error.hidden = !status.startsWith("failed");
  error.textContent = error.hidden ? "" : status.slice("failed: ".length);
  if (status === "downloading") button.textContent = "Downloading update…";
  else if (status === "installing") button.textContent = "Installing update…";
  else if (status.startsWith("failed")) button.textContent = "Update failed. Try again";
  else if (update.installable) button.textContent = `Update to Stillwater ${update.version}`;
  else button.textContent = `Stillwater ${update.version} is available`;
}

// The shield in the address bar: how many requests were blocked on this page.
function renderShield() {
  const adblock = state.adblock || {};
  const shield = $("shield");
  shield.hidden = !adblock.enabled || !adblock.host;
  shield.classList.toggle("off", !adblock.site);
  $("shield-count").textContent = adblock.site && adblock.blocked ? String(adblock.blocked) : "";
  shield.title = adblock.site
    ? ` blocked on this page`
    : `Ad blocking is off for ${adblock.host}`;
}

function shieldMenu() {
  const adblock = state.adblock || {};
  return [
    adblock.site
      ? { label: "Turn off for this site", run: () => send("adblock-site") }
      : { label: "Turn back on for this site", run: () => send("adblock-site") },
    "-",
    { label: "Ad blocking settings", run: () => send("open-settings", "privacy") },
  ];
}

// The translate button: pages in another language are translated on this computer (Firefox
// Translations' models, run by Stillwater's own engine), automatically unless turned off.
function renderTranslate() {
  const translate = state.translate || {};
  const button = $("translate");
  const status = translate.status || "idle";
  button.hidden = status === "idle" || status === "same";
  button.classList.toggle("translated", status === "translated");
  button.classList.toggle("busy", status === "downloading" || status === "translating");
  button.classList.toggle("problem", status === "failed" || status === "unsupported");
  const from = translate.fromName || "another language";
  const to = translate.toName || "your language";
  button.title = {
    offer: `Translate from ${from} to ${to}`,
    downloading: `Downloading ${from} for translation…`,
    translating: `Translating from ${from}…`,
    translated: `Translated from ${from}. Click to show the original`,
    unsupported: `${from} can't be translated to ${to} yet`,
    failed: translate.message || "Translation failed",
  }[status] || "Translate this page";
}

function translateMenu() {
  const translate = state.translate || {};
  const status = translate.status;
  const from = translate.fromName || "this language";
  const to = translate.toName || "your language";
  const items = [];
  if (status === "offer") {
    items.push({ note: `This page is in ${from}.` });
    items.push({ label: `Translate to ${to}`, glyph: "\uF2B7", run: () => send("tr-translate") });
  } else if (status === "downloading") {
    items.push({ note: `Downloading the ${from} translation model (about 30 MB, once)…` });
  } else if (status === "translating" || status === "translated") {
    items.push({ note: status === "translating" ? `Translating from ${from} to ${to}…` : `Translated from ${from} to ${to}, on this computer.` });
    items.push({ label: "Show original", glyph: "\uE7A7", run: () => send("tr-original") });
  } else if (status === "unsupported") {
    items.push({ note: `There's no translation model from ${from} to ${to} yet.` });
  } else if (status === "failed") {
    items.push({ note: translate.message || "Translation failed." });
    items.push({ label: "Try again", glyph: "\uE72C", run: () => send("tr-translate") });
  }
  if (translate.from) {
    items.push("-");
    if (!translate.auto) {
      items.push({ label: `${translate.alwaysThis ? "✓ " : ""}Always translate ${from}`, run: () => send("tr-always") });
    }
    items.push({ label: `${translate.neverThis ? "✓ " : ""}Never translate ${from}`, run: () => send("tr-never-language") });
    if (translate.host) {
      items.push({ label: `${translate.neverSite ? "✓ " : ""}Never translate ${translate.host}`, run: () => send("tr-never-site") });
    }
  }
  items.push("-");
  items.push({ label: "Translation settings", run: () => send("open-settings", "translation") });
  return items;
}

// The app menu (the ≡ button), like Zen's: everything Stillwater can open, with shortcuts.
function appMenu() {
  const version = (state.settings && state.settings.version) || "";
  const page = /^https?:/.test(state.url || "");
  return [
    { header: `Stillwater ${version}` },
    { label: "New tab", glyph: "\uE710", shortcut: "Ctrl+T", run: () => send("new-tab") },
    { label: "Reopen closed tab", glyph: "\uE7A7", run: () => send("session-reopen") },
    "-",
    { label: "History", glyph: "\uE81C", shortcut: "Ctrl+H", run: () => send("hist-show", "history") },
    { label: "Bookmarks", glyph: "\uE728", run: () => send("hist-show", "bookmarks") },
    { label: "Downloads", glyph: "\uE896", shortcut: "Ctrl+J", run: () => send("dl-panel") },
    { label: "Passwords", glyph: "\uE8D7", run: openPasswords },
    "-",
    ...(page ? [
      { label: "Bookmark this page", glyph: "\uE734", shortcut: "Ctrl+D", run: () => send("bm-star") },
      { label: "Compare with Edge", glyph: "\uE8F9", run: () => send("compare") },
      { label: "Report a broken site", glyph: "\uEBE8", run: () => send("report-site", "") },
      "-",
    ] : []),
    { label: "Settings", glyph: "\uE713", shortcut: "Ctrl+,", run: () => send("open-settings") },
    { label: "Check for updates", glyph: "\uE895", run: () => send("check-updates") },
    "-",
    { label: "Quit Stillwater", glyph: "\uE7E8", shortcut: "Ctrl+Q", run: () => send("win-close") },
  ];
}

// ---- Saved passwords ----

const passwordsPanel = $("passwords");
const passwordSearch = $("passwords-search");
// The login whose details are open in the list, and its password once shown.
let openLogin = null;
let shownPassword = null;
let hidePasswordTimer = 0;

function passwords() {
  return state.passwords || { list: [], site: [], never: [] };
}

function renderPasswordOffer() {
  const offer = passwords().offer;
  const card = $("pw-offer");
  card.hidden = !offer;
  if (!offer) return;
  $("pw-offer-title").textContent = offer.update
    ? `Update your password for ${offer.host}?`
    : `Save your password for ${offer.host}?`;
  $("pw-offer-user").textContent = offer.username || "No username";
  $("pw-offer-save").textContent = offer.update ? "Update" : "Save";
}

function renderKey() {
  const site = passwords().site || [];
  const key = $("key");
  key.hidden = site.length === 0;
  key.title = site.length === 1 ? "Fill your saved password" : `${site.length} saved passwords for this site`;
}

function keyMenu() {
  const items = (passwords().site || []).map((login) => ({
    label: `Fill ${login.username || "password (no username)"}`,
    run: () => send("pw-fill", login.id),
  }));
  items.push("-", { label: "Saved passwords", run: openPasswords });
  return items;
}

function flash(button, text) {
  const original = button.textContent;
  button.textContent = text;
  setTimeout(() => { button.textContent = original; }, 1200);
}

function loginDetails(login) {
  const details = document.createElement("div");
  details.className = "login-details";
  const secret = document.createElement("div");
  secret.className = "login-secret";
  secret.textContent = shownPassword !== null ? shownPassword : "••••••••••";
  const actions = document.createElement("div");
  actions.className = "login-actions";
  const action = (label, onClick) => {
    const button = document.createElement("button");
    button.className = "secondary small";
    button.textContent = label;
    button.addEventListener("click", (event) => { event.stopPropagation(); onClick(button); });
    actions.append(button);
    return button;
  };
  action(shownPassword !== null ? "Hide" : "Show", () => {
    if (shownPassword !== null) {
      shownPassword = null;
      renderPasswordList();
    } else {
      send("pw-reveal", login.id);
    }
  });
  action("Copy password", (button) => { send("pw-copy", login.id); flash(button, "Copied"); });
  if (login.username) {
    action("Copy username", (button) => { send("pw-copy-user", login.id); flash(button, "Copied"); });
  }
  if ((passwords().site || []).some((item) => item.id === login.id)) {
    action("Fill", () => send("pw-fill", login.id));
  }
  // Deleting can't be undone, so it takes a second click.
  action("Delete", (button) => {
    if (button.classList.contains("armed")) {
      send("pw-delete", login.id);
      openLogin = null;
      return;
    }
    button.classList.add("armed");
    button.textContent = "Click again to delete";
    setTimeout(() => { button.classList.remove("armed"); button.textContent = "Delete"; }, 3000);
  });
  details.append(secret, actions);
  return details;
}

function renderPasswordList() {
  const data = passwords();
  const query = passwordSearch.value.trim().toLowerCase();
  const list = (data.list || []).filter((login) =>
    !query || login.host.toLowerCase().includes(query) || login.username.toLowerCase().includes(query));
  $("passwords-list").replaceChildren(...list.map((login) => {
    const row = document.createElement("div");
    row.className = "login";
    row.classList.toggle("open", openLogin === login.id);
    const head = document.createElement("button");
    head.className = "login-head";
    const host = document.createElement("span");
    host.className = "login-host";
    host.textContent = login.host;
    const user = document.createElement("span");
    user.className = "login-user";
    user.textContent = login.username || "No username";
    head.append(host, user);
    head.title = login.origin;
    head.addEventListener("click", () => {
      openLogin = openLogin === login.id ? null : login.id;
      shownPassword = null;
      renderPasswordList();
    });
    row.append(head);
    if (openLogin === login.id) row.append(loginDetails(login));
    return row;
  }));
  const empty = $("passwords-empty");
  empty.hidden = list.length > 0;
  empty.textContent = query
    ? "No saved passwords match."
    : "No saved passwords yet. Stillwater offers to save one when you sign in to a site, or you can import them from Zen in settings.";
  const problem = $("passwords-problem");
  problem.hidden = !data.problem;
  problem.textContent = data.problem || "";
  const never = data.never || [];
  $("never-group").hidden = never.length === 0;
  $("never-list").replaceChildren(...never.map((site) => {
    const row = document.createElement("div");
    row.className = "login never";
    const name = document.createElement("span");
    name.className = "login-host";
    name.textContent = site.host;
    const remove = iconButton("never-remove", "\uE711", "Offer to save passwords for this site again",
      () => send("pw-forget-never", site.origin));
    row.append(name, remove);
    return row;
  }));
}

function openPasswords() {
  passwordsPanel.hidden = false;
  passwordSearch.value = "";
  passwordSearch.parentElement.classList.remove("has-value");
  openLogin = null;
  shownPassword = null;
  renderPasswordList();
}

function closePasswords() {
  passwordsPanel.hidden = true;
  openLogin = null;
  shownPassword = null;
}

// Compare with Edge: show progress; a finished message goes away after a few seconds.
let compareHideTimer = 0;
let lastCompareMessage = "";
function renderCompare() {
  const compare = state.compare || {};
  const line = $("compare-status");
  const message = compare.message || "";
  if (message !== lastCompareMessage) {
    lastCompareMessage = message;
    clearTimeout(compareHideTimer);
    line.hidden = !message;
    if (message && !compare.busy) compareHideTimer = setTimeout(() => { line.hidden = true; }, compare.error ? 10000 : 5000);
  }
  line.textContent = message;
  line.classList.toggle("error", !!compare.error);
}

function render() {
  renderCompare();
  renderPasswordOffer();
  renderKey();
  if (!passwordsPanel.hidden) renderPasswordList();
  renderShield();
  renderTranslate();
  renderUpdate();
  if (state.workspaces) renderWorkspaces();
  // Don't rebuild the lists while the user is renaming something in them.
  if (!document.querySelector(".rename-input")) {
    renderEssentials();
    renderPinned();
    const rows = state.tabs.map((tab) => renderRow(tab, false));
    if (state.split) rows.push(renderSplitRow(state.split));
    tabs.replaceChildren(...rows);
  }
  $("back").disabled = !state.canGoBack;
  $("forward").disabled = !state.canGoForward;
  $("compact").textContent = state.compact ? "" : "";
  document.body.classList.toggle("compact", state.compact);
  if (document.activeElement !== url) showUrlHost();
  syncPlaceholder();
}

// The address to edit: the page's full URL (empty on the new tab page).
function fullUrl() {
  return state.url === "servo:newtab" ? "" : state.url;
}

// What the address bar shows while it isn't being edited: only the site's host, so that a
// long `https://bank.com.evil.io/…` or `https://bank.com@evil.io/` can't pass for the real
// site in the narrow sidebar. `https:` is left out, plain `http://` is always shown, and the
// user name/password part of a URL never is. Internal pages show as before.
function hostForDisplay(text) {
  let parsed;
  try {
    parsed = new URL(text);
  } catch (_) {
    return text;
  }
  if (!parsed.host) return text;
  if (parsed.protocol === "https:") return parsed.host;
  if (parsed.protocol === "http:") return `http://${parsed.host}`;
  return text;
}

// Whether the field shows `hostForDisplay` rather than text to edit.
let urlShowsHost = false;

function showUrlHost() {
  const host = hostForDisplay(fullUrl());
  urlShowsHost = host !== fullUrl();
  // The left-to-right marks keep `[::1]`-style hosts in order in the right-to-left field.
  url.value = urlShowsHost ? `\u200E${host}\u200E` : host;
  urlbar.classList.toggle("shows-host", urlShowsHost);
  syncPlaceholder();
}

// Editing starts from the full URL.
function showUrlForEditing() {
  if (!urlShowsHost) return;
  urlShowsHost = false;
  urlbar.classList.remove("shows-host");
  url.value = fullUrl();
  syncPlaceholder();
}

window.sidebar = {
  setState(newState) {
    state = newState;
    render();
  },
  // Settings are a page in a tab now.
  openSettings() {
    send("open-settings");
  },
  // A saved password the user asked to see (sent only on request, never in the state).
  revealPassword(id, password) {
    if (openLogin !== id || passwordsPanel.hidden) return;
    shownPassword = password;
    renderPasswordList();
    clearTimeout(hidePasswordTimer);
    hidePasswordTimer = setTimeout(() => { shownPassword = null; renderPasswordList(); }, 30000);
  },
  focusUrl() {
    url.focus();
    // Selecting an empty field would highlight its placeholder in Servo.
    if (url.value) url.select();
  },
};

function syncPlaceholder() {
  url.parentElement.classList.toggle("has-value", url.value.length > 0);
}
url.addEventListener("input", syncPlaceholder);
url.addEventListener("focus", () => {
  urlbar.classList.add("focused");
  if (urlShowsHost) {
    showUrlForEditing();
    if (url.value) url.select();
  }
});
url.addEventListener("blur", () => {
  urlbar.classList.remove("focused");
  // Checked once the focus has moved. Still the focused element means only the window lost
  // focus, so keep what the user typed.
  setTimeout(() => {
    if (document.activeElement !== url) showUrlHost();
  }, 0);
});
url.addEventListener("keydown", (event) => {
  if (event.key === "Enter" && url.value.trim()) {
    send("go", url.value.trim());
    url.blur();
  } else if (event.key === "Escape") {
    url.value = state.url;
    url.blur();
    send("blur");
  }
});

$("back").addEventListener("click", () => send("back"));
$("forward").addEventListener("click", () => send("forward"));
$("reload").addEventListener("click", () => send("reload"));
$("compact").addEventListener("click", () => send("toggle-compact"));
$("new-tab").addEventListener("click", () => send("new-tab"));
$("ws-new-folder").addEventListener("click", () => send("folder-new", 0));

// Frameless window: caption buttons, and dragging/double-clicking empty sidebar space.
update.addEventListener("click", () => send("update"));

// The app menu (≡), under its button.
$("main-menu").addEventListener("click", (event) => {
  event.stopPropagation();
  const box = $("main-menu").getBoundingClientRect();
  openMenu(appMenu(), box.left, box.bottom + 4);
});
$("translate").addEventListener("click", (event) => {
  event.stopPropagation();
  const box = $("translate").getBoundingClientRect();
  openMenu(translateMenu(), box.left, box.bottom);
});
$("shield").addEventListener("click", (event) => {
  event.stopPropagation();
  const box = $("shield").getBoundingClientRect();
  openMenu(shieldMenu(), box.left, box.bottom);
});
const isDragSurface = (target) =>
  target.matches("[data-drag], body, #tab-area, #tabs, #pinned, #essentials, #footer, #workspaces, #workspace-header, .spacer");
let lastDragDown = 0;
document.addEventListener("mousedown", (event) => {
  if (event.button !== 0 || !state.frameless || !isDragSurface(event.target)) return;
  const now = Date.now();
  if (now - lastDragDown < 400) {
    lastDragDown = 0;
    send("win-max"); // double-click toggles maximise, like a title bar
  } else {
    lastDragDown = now;
    send("win-drag");
  }
});

// Workspace header: click icon / colour dot to cycle them, click the name to rename.
$("ws-icon").addEventListener("click", () => send("ws-icon", currentWorkspace().id));
$("ws-color").addEventListener("click", () => send("ws-color", currentWorkspace().id));

const nameButton = $("ws-name");
const nameInput = $("ws-name-input");
function finishRename(commit) {
  if (nameInput.hidden) return;
  if (commit && nameInput.value.trim()) {
    send("ws-rename", `${currentWorkspace().id}\t${nameInput.value.trim()}`);
  }
  nameInput.hidden = true;
  nameButton.hidden = false;
}
// Double-click to rename (like Zen), so a stray click doesn't open the editor.
// Timed by hand rather than with the dblclick event, which Servo may not fire.
let lastNameClick = 0;
nameButton.addEventListener("click", () => {
  const now = Date.now();
  const isDouble = now - lastNameClick < 400;
  lastNameClick = isDouble ? 0 : now;
  if (!isDouble) return;
  nameInput.value = currentWorkspace().name;
  nameButton.hidden = true;
  nameInput.hidden = false;
  nameInput.focus();
  nameInput.select();
});
nameInput.addEventListener("keydown", (event) => {
  if (event.key === "Enter") finishRename(true);
  else if (event.key === "Escape") finishRename(false);
});
nameInput.addEventListener("blur", () => finishRename(true));

// Deleting a space closes its tabs, so ask for a second click (no confirm() in the sidebar).
const deleteButton = $("ws-delete");
let disarmTimer = 0;
deleteButton.addEventListener("click", () => {
  if (deleteButton.classList.contains("armed")) {
    clearTimeout(disarmTimer);
    deleteButton.classList.remove("armed");
    send("ws-delete", currentWorkspace().id);
    return;
  }
  deleteButton.classList.add("armed");
  deleteButton.title = "Click again to delete this space and its tabs";
  disarmTimer = setTimeout(() => {
    deleteButton.classList.remove("armed");
    deleteButton.title = "Delete space";
  }, 3000);
});

// Passwords
$("pw-offer-save").addEventListener("click", () => send("pw-save"));
$("pw-offer-never").addEventListener("click", () => send("pw-never"));
$("pw-offer-dismiss").addEventListener("click", () => send("pw-dismiss"));
$("key").addEventListener("click", (event) => {
  event.stopPropagation();
  const box = $("key").getBoundingClientRect();
  openMenu(keyMenu(), box.left, box.bottom);
});
$("passwords-close").addEventListener("click", closePasswords);
passwordSearch.addEventListener("input", () => {
  passwordSearch.parentElement.classList.toggle("has-value", passwordSearch.value.length > 0);
  renderPasswordList();
});
document.addEventListener("keydown", (event) => {
  if (event.key === "Escape" && !passwordsPanel.hidden) closePasswords();
});

send("ready");
