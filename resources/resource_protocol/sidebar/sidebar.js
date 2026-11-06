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

function renderTab(tab) {
  const row = document.createElement("div");
  row.className = "tab" + (tab.active ? " selected" : "") + (tab.loading ? " loading" : "");
  row.title = tab.url;

  const label = document.createElement("span");
  label.className = "tab-label";
  label.textContent = tabLabel(tab);

  const close = document.createElement("button");
  close.className = "tab-close icon";
  close.title = "Close tab";
  close.textContent = "";
  close.addEventListener("click", (event) => {
    event.stopPropagation();
    send("close", tab.id);
  });

  row.append(tabIcon(tab), label, close);
  row.addEventListener("click", () => send("activate", tab.id));
  row.addEventListener("mouseup", (event) => {
    if (event.button === 1) send("close", tab.id); // middle click
  });
  return row;
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

function renderCaption() {
  $("caption").hidden = !state.frameless;
  const max = $("win-max");
  max.textContent = state.maximized ? "" : "";
  max.title = state.maximized ? "Restore" : "Maximise";
}

function renderUpdate() {
  const button = $("update");
  const update = state.update;
  button.hidden = !update;
  if (!update) return;
  const status = update.status;
  button.disabled = status === "downloading" || status === "installing";
  button.classList.toggle("failed", status.startsWith("failed"));
  button.title = status.startsWith("failed") ? status.slice("failed: ".length) : "";
  if (status === "downloading") button.textContent = "Downloading update…";
  else if (status === "installing") button.textContent = "Installing update…";
  else if (status.startsWith("failed")) button.textContent = "Update failed. Try again";
  else if (update.installable) button.textContent = `Update to Stillwater ${update.version}`;
  else button.textContent = `Stillwater ${update.version} is available`;
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
  $("version-line").textContent = `Stillwater ${settings.version}`;
  $("update-check").textContent = state.updateCheck || "";
}

function render() {
  renderCaption();
  renderSettings();
  renderUpdate();
  if (state.workspaces) renderWorkspaces();
  tabs.replaceChildren(...state.tabs.map(renderTab));
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

// Frameless window: caption buttons, and dragging/double-clicking empty sidebar space.
update.addEventListener("click", () => send("update"));

// Settings panel (the gear in the bottom row).
const settingsPanel = $("settings");
$("settings-button").addEventListener("click", () => { settingsPanel.hidden = false; });
$("settings-close").addEventListener("click", () => { settingsPanel.hidden = true; });
document.addEventListener("keydown", (event) => {
  if (event.key === "Escape" && !settingsPanel.hidden) settingsPanel.hidden = true;
});
document.querySelectorAll("#theme-choice button").forEach((button) => {
  button.addEventListener("click", () => send("set-theme", button.dataset.theme));
});
$("compact-toggle").addEventListener("click", () => send("toggle-compact"));
$("check-updates").addEventListener("click", () => send("check-updates"));
$("win-min").addEventListener("click", () => send("win-min"));
$("win-max").addEventListener("click", () => send("win-max"));
$("win-close").addEventListener("click", () => send("win-close"));

const isDragSurface = (target) =>
  target.matches("[data-drag], body, #tabs, #footer, #workspaces, #workspace-header, .spacer");
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

send("ready");
