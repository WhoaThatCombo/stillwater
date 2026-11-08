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
    const button = document.createElement("button");
    button.className = "menu-item";
    button.textContent = item.label;
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
    { label: "Report site isn't working", run: () => send("report-site", tab.id) },
    "-",
    ...(tab.loaded && !tab.active ? [{ label: "Unload (free memory)", run: () => send("tab-unload", tab.id) }] : []),
    { label: "Close tab", run: () => send("close", tab.id) },
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

  const strip = state.workspaces.list.map((space, index) => {
    const button = document.createElement("button");
    button.className = "workspace" + (space.id === workspace.id ? " active" : "");
    button.title = `${space.name} (Ctrl+Alt+${index + 1})`;
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
    { label: "Ad blocking settings", run: () => send("open-settings") },
  ];
}

function renderSettings() {
  const settings = state.settings;
  if (!settings) return;
  document.querySelectorAll("#theme-choice button").forEach((button) => {
    button.setAttribute("aria-pressed", String(button.dataset.theme === settings.theme));
  });
  $("search-choice").replaceChildren(...settings.searchEngines.map((engine) => {
    const button = document.createElement("button");
    button.className = "option";
    button.textContent = engine.name;
    button.setAttribute("aria-pressed", String(engine.id === settings.searchEngine));
    button.addEventListener("click", () => send("set-search", engine.id));
    return button;
  }));
  $("compact-toggle").setAttribute("aria-pressed", String(state.compact));
  $("unload-choice").replaceChildren(...(settings.tabUnloadChoices || []).map((minutes) => {
    const button = document.createElement("button");
    button.textContent = minutes === 0 ? "Off" : minutes < 60 ? `${minutes}m` : `${minutes / 60}h`;
    button.title = minutes === 0 ? "Never unload tabs on a timer" : `After ${minutes} minutes`;
    button.setAttribute("aria-pressed", String(minutes === settings.tabUnloadMinutes));
    button.addEventListener("click", () => send("set-tab-unload", minutes));
    return button;
  }));
  $("lowmem-toggle").setAttribute("aria-pressed", String(!!settings.unloadOnLowMemory));
  const adblock = state.adblock || {};
  $("adblock-toggle").setAttribute("aria-pressed", String(!!adblock.enabled));
  $("adblock-hint").textContent = adblock.enabled && !adblock.ready
    ? "Downloading filter lists…"
    : "Uses uBlock Origin's filter lists. Turn it off for one site with the shield in the address bar.";
  $("version-line").textContent = `Stillwater ${settings.version}`;
  $("update-check").textContent = state.updateCheck || "";
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

// ---- Import from Zen ----

const zenChoice = { profile: 0, pins: true, tabs: false, passwords: true };
let zenBusy = false;

function renderZen() {
  const zen = state.zen;
  const status = $("zen-status");
  const found = $("zen-found");
  if (!zen) {
    found.hidden = true;
    status.textContent = zenBusy ? "Looking for Zen…" : "Copies your spaces, pinned tabs, Essentials and passwords from Zen Browser on this computer. Zen isn't changed.";
    return;
  }
  zenBusy = false;
  const profiles = zen.profiles || [];
  if (profiles.length === 0) {
    found.hidden = true;
    status.textContent = "Zen Browser's data wasn't found on this computer.";
    return;
  }
  found.hidden = false;
  zenChoice.profile = zen.profile || 0;
  $("zen-profiles").hidden = profiles.length < 2;
  $("zen-profiles").replaceChildren(...profiles.map((name, index) => {
    const button = document.createElement("button");
    button.className = "option";
    button.textContent = name;
    button.setAttribute("aria-pressed", String(index === zenChoice.profile));
    button.addEventListener("click", () => send("zen-scan", index));
    return button;
  }));
  const count = (n, one, many) => `${n} ${n === 1 ? one : many}`;
  $("zen-pins-count").textContent = zen.sessionProblem ||
    `${count(zen.spaces, "space", "spaces")}, ${count(zen.pins, "pinned tab", "pinned tabs")}, ${count(zen.essentials, "Essential", "Essentials")}`;
  $("zen-tabs-count").textContent = zen.sessionProblem ? "" : `${count(zen.tabs, "tab", "tabs")}, added unloaded`;
  $("zen-passwords-count").textContent = zen.passwordProblem || count(zen.passwords, "password", "passwords");
  for (const what of ["pins", "tabs", "passwords"]) {
    $(`zen-${what}`).setAttribute("aria-pressed", String(zenChoice[what]));
  }
  $("zen-import").disabled = !(zenChoice.pins || zenChoice.tabs || zenChoice.passwords);
  status.textContent = zen.result || "";
}

function render() {
  renderSettings();
  renderPasswordOffer();
  renderKey();
  renderZen();
  if (!passwordsPanel.hidden) renderPasswordList();
  $("autofill-toggle").setAttribute("aria-pressed", String(!!passwords().autofill));
  $("open-passwords").textContent = `Saved passwords (${(passwords().list || []).length})`;
  renderShield();
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
  if (document.activeElement !== url) {
    url.value = state.url === "servo:newtab" ? "" : state.url;
  }
  syncPlaceholder();
}

window.sidebar = {
  setState(newState) {
    state = newState;
    render();
  },
  openSettings() {
    document.getElementById("settings").hidden = false;
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
url.addEventListener("focus", () => urlbar.classList.add("focused"));
url.addEventListener("blur", () => urlbar.classList.remove("focused"));
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

// Settings panel (the gear in the bottom row).
const settingsPanel = $("settings");
$("settings-button").addEventListener("click", () => { settingsPanel.hidden = false; });
$("main-menu").addEventListener("click", () => { settingsPanel.hidden = false; });
$("settings-close").addEventListener("click", () => { settingsPanel.hidden = true; });
document.addEventListener("keydown", (event) => {
  if (event.key === "Escape" && !settingsPanel.hidden) settingsPanel.hidden = true;
});
document.querySelectorAll("#theme-choice button").forEach((button) => {
  button.addEventListener("click", () => send("set-theme", button.dataset.theme));
});
$("compact-toggle").addEventListener("click", () => send("toggle-compact"));
$("lowmem-toggle").addEventListener("click", () => send("set-unload-low-memory", state.settings && state.settings.unloadOnLowMemory ? "0" : "1"));
$("adblock-toggle").addEventListener("click", () => send("adblock-enabled", (state.adblock && state.adblock.enabled) ? "0" : "1"));
$("shield").addEventListener("click", (event) => {
  event.stopPropagation();
  const box = $("shield").getBoundingClientRect();
  openMenu(shieldMenu(), box.left, box.bottom);
});
$("check-updates").addEventListener("click", () => send("check-updates"));
$("report-site").addEventListener("click", () => send("report-site", ""));

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
$("autofill-toggle").addEventListener("click", () => send("pw-autofill", passwords().autofill ? "0" : "1"));
$("open-passwords").addEventListener("click", openPasswords);
$("passwords-close").addEventListener("click", closePasswords);
passwordSearch.addEventListener("input", () => {
  passwordSearch.parentElement.classList.toggle("has-value", passwordSearch.value.length > 0);
  renderPasswordList();
});
document.addEventListener("keydown", (event) => {
  if (event.key === "Escape" && !passwordsPanel.hidden) closePasswords();
});

// Import from Zen
$("zen-scan").addEventListener("click", () => {
  zenBusy = true;
  state.zen = null;
  renderZen();
  send("zen-scan", zenChoice.profile);
});
for (const what of ["pins", "tabs", "passwords"]) {
  $(`zen-${what}`).addEventListener("click", () => {
    zenChoice[what] = !zenChoice[what];
    renderZen();
  });
}
$("zen-import").addEventListener("click", () => {
  const what = ["pins", "tabs", "passwords"].filter((name) => zenChoice[name]);
  if (what.length === 0) return;
  $("zen-status").textContent = "Importing…";
  send("zen-import", `${zenChoice.profile}\t${what.join(",")}`);
});

send("ready");
