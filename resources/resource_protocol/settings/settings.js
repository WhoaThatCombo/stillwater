/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

// Stillwater's settings page. Talks to Stillwater (desktop/sidebar.rs) like the sidebar does:
//   page -> Rust: console.log("__stillwater_settings__<command>\t<argument>"); Stillwater only
//                 accepts these from a tab showing servo:settings, and only settings commands.
//   Rust -> page: window.stillwaterSettings.setState(state), revealPassword(id, password),
//                 histCleared(message).

"use strict";

const send = (command, argument = "") => console.log(`__stillwater_settings__${command}\t${argument}`);
const $ = (id) => document.getElementById(id);
let state = {};

// ---- Categories and search ----

const panes = [...document.querySelectorAll(".pane")];
const categories = [...document.querySelectorAll(".category")];
function showPane(name) {
  if (!panes.some((pane) => pane.dataset.pane === name)) name = "general";
  for (const pane of panes) pane.hidden = pane.dataset.pane !== name;
  for (const category of categories) category.setAttribute("aria-current", String(category.dataset.pane === name));
  document.querySelectorAll(".group").forEach((group) => { group.hidden = group.id === "never-group" && !neverShown(); });
  $("no-results").hidden = true;
  if (location.hash !== "#" + name) history.replaceState(null, "", "#" + name);
  $("main").scrollTop = 0;
}
for (const category of categories) {
  category.addEventListener("click", () => {
    $("search").value = "";
    showPane(category.dataset.pane);
  });
}
window.addEventListener("hashchange", () => showPane(location.hash.slice(1)));

// Find in settings: every group whose text matches, from any category.
$("search").addEventListener("input", () => {
  const query = $("search").value.trim().toLowerCase();
  if (!query) {
    showPane((location.hash || "#general").slice(1));
    return;
  }
  let matches = 0;
  for (const pane of panes) {
    let paneMatches = 0;
    for (const group of pane.querySelectorAll(".group")) {
      const text = (group.textContent + " " + (group.dataset.words || "") + " " + pane.querySelector("h1").textContent).toLowerCase();
      const show = text.includes(query) && !(group.id === "never-group" && !neverShown());
      group.hidden = !show;
      if (show) paneMatches++;
    }
    pane.hidden = paneMatches === 0;
    matches += paneMatches;
  }
  for (const category of categories) category.setAttribute("aria-current", "false");
  $("no-results").hidden = matches > 0;
});

// ---- Small helpers ----

function pressed(element, on) {
  element.setAttribute("aria-pressed", String(!!on));
}
function choices(container, items, current, onPick) {
  container.replaceChildren(...items.map((item) => {
    const button = document.createElement("button");
    button.textContent = item.label;
    if (item.detail) {
      const detail = document.createElement("span");
      detail.textContent = item.detail;
      button.append(detail);
    }
    pressed(button, item.value === current);
    button.addEventListener("click", () => onPick(item.value));
    return button;
  }));
}
function wireChoices(container, onPick) {
  container.querySelectorAll("button").forEach((button) => button.addEventListener("click", () => onPick(button.dataset.value)));
}
function markChoices(container, current) {
  container.querySelectorAll("button").forEach((button) => pressed(button, button.dataset.value === String(current)));
}
const count = (n, one, many) => `${n} ${n === 1 ? one : many}`;

// ---- General, search, tabs ----

wireChoices($("theme"), (value) => send("set-theme", value));
wireChoices($("startup"), (value) => send("session-startup", value));
wireChoices($("newtab"), (value) => send("set-newtab", value));
$("compact").addEventListener("click", () => send("toggle-compact"));
$("lowmem").addEventListener("click", () => send("set-unload-low-memory", state.settings && state.settings.unloadOnLowMemory ? "0" : "1"));
$("splash").addEventListener("click", () => send("set-splash", state.settings && state.settings.startupAnimation === false ? "1" : "0"));
$("spaces").addEventListener("click", () => send("set-spaces", state.settings && state.settings.spaces ? "0" : "1"));
$("adblock").addEventListener("click", () => send("adblock-enabled", state.adblock && state.adblock.enabled ? "0" : "1"));

function renderGeneral() {
  const settings = state.settings || {};
  markChoices($("theme"), settings.theme || "system");
  markChoices($("startup"), settings.startup || "restore");
  markChoices($("newtab"), settings.newTabBar === false ? "0" : "1");
  pressed($("compact"), state.compact);
  choices($("engines"), (settings.searchEngines || []).map((engine) => ({ label: engine.name, value: engine.id })),
    settings.searchEngine, (id) => send("set-search", id));
  const minutes = settings.tabUnloadChoices || [];
  choices($("unload"), minutes.map((value) => ({
    label: value === 0 ? "Never" : value < 60 ? `${value} min` : `${value / 60} h`, value,
  })), settings.tabUnloadMinutes, (value) => send("set-tab-unload", value));
  pressed($("lowmem"), settings.unloadOnLowMemory);
  pressed($("spaces"), settings.spaces);
  pressed($("splash"), settings.startupAnimation !== false);
  const adblock = state.adblock || {};
  pressed($("adblock"), adblock.enabled);
  $("adblock-hint").textContent = adblock.enabled && !adblock.ready
    ? "Downloading filter lists…"
    : "Uses uBlock Origin's filter lists. Turn it off for one site with the shield in the address bar.";
  $("version").textContent = `Stillwater ${settings.version || ""}`;
  $("update-status").textContent = state.updateCheck || "";
  // Tint like the rest of Stillwater: the current space's colour.
  const workspaces = state.workspaces || {};
  const space = (workspaces.list || []).find((item) => item.id === workspaces.current);
  document.documentElement.style.setProperty("--accent", (space && space.color) || "");
}

// ---- Privacy ----

let clearRange = "hour";
let clearSiteData = false;
wireChoices($("clear-range"), (value) => { clearRange = value; markChoices($("clear-range"), clearRange); });
markChoices($("clear-range"), clearRange);
$("clear-site-data").addEventListener("click", () => { clearSiteData = !clearSiteData; pressed($("clear-site-data"), clearSiteData); });
pressed($("clear-site-data"), false);
function clearSince(range) {
  if (range === "hour") return Date.now() - 3600 * 1000;
  if (range === "today") {
    const midnight = new Date();
    midnight.setHours(0, 0, 0, 0);
    return midnight.getTime();
  }
  return 0;
}
// Clearing can't be undone, so it takes a second click.
let clearArmed = 0;
$("clear").addEventListener("click", () => {
  const button = $("clear");
  if (!clearArmed) {
    button.textContent = clearSiteData ? "Click again to clear history, cookies and site data" : "Click again to clear history";
    button.classList.add("armed");
    clearArmed = setTimeout(() => { clearArmed = 0; button.textContent = "Clear history"; button.classList.remove("armed"); }, 4000);
    return;
  }
  clearTimeout(clearArmed);
  clearArmed = 0;
  button.textContent = "Clear history";
  button.classList.remove("armed");
  $("clear-status").textContent = "Clearing…";
  send("hist-clear", `${clearSince(clearRange)}\t${clearSiteData ? 1 : 0}`);
});
$("show-history").addEventListener("click", () => send("hist-show", "history"));
$("show-bookmarks").addEventListener("click", () => send("hist-show", "bookmarks"));

// ---- Passwords ----

let openLogin = null;
let shownPassword = null;
let hideTimer = 0;
const passwords = () => state.passwords || { list: [], never: [] };
const neverShown = () => (passwords().never || []).length > 0;
$("autofill").addEventListener("click", () => send("pw-autofill", passwords().autofill ? "0" : "1"));
$("login-search").addEventListener("input", renderLogins);

function flash(button, text) {
  const original = button.textContent;
  button.textContent = text;
  setTimeout(() => { button.textContent = original; }, 1200);
}
function loginRow(login) {
  const row = document.createElement("div");
  row.className = "login" + (openLogin === login.id ? " open" : "");
  const head = document.createElement("button");
  head.className = "login-head";
  head.title = login.origin;
  const host = document.createElement("span");
  host.className = "login-host";
  host.textContent = login.host;
  const user = document.createElement("span");
  user.className = "login-user";
  user.textContent = login.username || "No username";
  head.append(host, user);
  head.addEventListener("click", () => {
    openLogin = openLogin === login.id ? null : login.id;
    shownPassword = null;
    renderLogins();
  });
  row.append(head);
  if (openLogin !== login.id) return row;
  const details = document.createElement("div");
  details.className = "login-details";
  const secret = document.createElement("code");
  secret.className = "secret";
  secret.textContent = shownPassword !== null ? shownPassword : "••••••••••";
  const actions = document.createElement("div");
  actions.className = "row";
  const action = (label, onClick, className = "secondary") => {
    const button = document.createElement("button");
    button.className = className;
    button.textContent = label;
    button.addEventListener("click", () => onClick(button));
    actions.append(button);
  };
  action(shownPassword !== null ? "Hide" : "Show", () => {
    if (shownPassword !== null) { shownPassword = null; renderLogins(); } else send("pw-reveal", login.id);
  });
  action("Copy password", (button) => { send("pw-copy", login.id); flash(button, "Copied"); });
  if (login.username) action("Copy username", (button) => { send("pw-copy-user", login.id); flash(button, "Copied"); });
  action("Delete", (button) => {
    if (button.classList.contains("armed")) { send("pw-delete", login.id); openLogin = null; return; }
    button.classList.add("armed");
    button.textContent = "Click again to delete";
    setTimeout(() => { button.classList.remove("armed"); button.textContent = "Delete"; }, 3000);
  });
  details.append(secret, actions);
  row.append(details);
  return row;
}
function renderLogins() {
  const data = passwords();
  pressed($("autofill"), data.autofill);
  const query = $("login-search").value.trim().toLowerCase();
  const list = (data.list || []).filter((login) =>
    !query || login.host.toLowerCase().includes(query) || login.username.toLowerCase().includes(query));
  $("logins").replaceChildren(...list.map(loginRow));
  $("logins-empty").hidden = list.length > 0;
  $("logins-empty").textContent = query
    ? "No saved passwords match."
    : "No saved passwords yet. Stillwater offers to save one when you sign in to a site, or you can import them from Zen.";
  $("logins-problem").hidden = !data.problem;
  $("logins-problem").textContent = data.problem || "";
  const never = data.never || [];
  $("never").replaceChildren(...never.map((site) => {
    const row = document.createElement("div");
    row.className = "login never";
    const name = document.createElement("span");
    name.className = "login-host";
    name.textContent = site.host;
    const remove = document.createElement("button");
    remove.className = "secondary";
    remove.textContent = "Allow saving";
    remove.addEventListener("click", () => send("pw-forget-never", site.origin));
    row.append(name, remove);
    return row;
  }));
  if (!$("search").value.trim()) $("never-group").hidden = never.length === 0;
}

// ---- Downloads ----

$("ask-where").addEventListener("click", () => send("dl-ask", state.downloads && state.downloads.askWhereToSave ? "0" : "1"));
$("show-downloads").addEventListener("click", () => send("dl-panel"));
function renderDownloads() {
  const data = state.downloads || {};
  pressed($("ask-where"), data.askWhereToSave);
  $("download-folder").textContent = data.askWhereToSave
    ? "Stillwater asks before each download."
    : `Files are saved to ${data.folder || "your Downloads folder"}.`;
}

// ---- Import from Zen ----

const zenKinds = ["pins", "tabs", "passwords", "bookmarks", "history", "cookies"];
const zenChoice = { profile: 0, pins: true, tabs: false, passwords: true, bookmarks: true, history: false, cookies: true };
let zenBusy = false;
$("zen-scan").addEventListener("click", () => {
  zenBusy = true;
  state.zen = null;
  renderZen();
  send("zen-scan", zenChoice.profile);
});
document.querySelectorAll("[data-zen]").forEach((toggle) => toggle.addEventListener("click", () => {
  zenChoice[toggle.dataset.zen] = !zenChoice[toggle.dataset.zen];
  renderZen();
}));
$("zen-import").addEventListener("click", () => {
  const what = zenKinds.filter((name) => zenChoice[name]);
  if (what.length === 0) return;
  $("zen-status").textContent = "Importing…";
  send("zen-import", `${zenChoice.profile}\t${what.join(",")}`);
});
function renderZen() {
  const zen = state.zen;
  if (!zen) {
    $("zen-found").hidden = true;
    $("zen-status").textContent = zenBusy ? "Looking for Zen…" : "";
    return;
  }
  zenBusy = false;
  const profiles = zen.profiles || [];
  if (profiles.length === 0) {
    $("zen-found").hidden = true;
    $("zen-status").textContent = "Zen Browser's data wasn't found on this computer.";
    return;
  }
  $("zen-found").hidden = false;
  zenChoice.profile = zen.profile || 0;
  $("zen-profiles").hidden = profiles.length < 2;
  choices($("zen-profiles"), profiles.map((name, index) => ({ label: name, value: index })), zenChoice.profile,
    (index) => send("zen-scan", index));
  const counts = {
    pins: zen.sessionProblem || `${count(zen.spaces, "space", "spaces")}, ${count(zen.pins, "pinned tab", "pinned tabs")}, ${count(zen.essentials, "Essential", "Essentials")}`,
    tabs: zen.sessionProblem ? "" : `${count(zen.tabs, "tab", "tabs")}, added unloaded`,
    passwords: zen.passwordProblem || count(zen.passwords, "password", "passwords"),
    bookmarks: zen.placesProblem || count(zen.bookmarks || 0, "bookmark", "bookmarks"),
    history: zen.placesProblem || `${count(zen.visits || 0, "visit", "visits")} in the last 90 days`,
    cookies: zen.cookiesProblem || `${count(zen.cookies || 0, "cookie", "cookies")}; replaces Stillwater's cookies for the same sites`,
  };
  document.querySelectorAll("[data-zen-count]").forEach((span) => { span.textContent = counts[span.dataset.zenCount]; });
  document.querySelectorAll("[data-zen]").forEach((toggle) => pressed(toggle, zenChoice[toggle.dataset.zen]));
  $("zen-import").disabled = !zenKinds.some((what) => zenChoice[what]);
  $("zen-status").textContent = zen.result || "";
}

// ---- Keyboard shortcuts (a reference) ----

const SHORTCUTS = [
  ["Ctrl+T", "New tab (the floating address bar, or a New Tab page)"],
  ["Ctrl+W", "Close tab"],
  ["Ctrl+Shift+T", "Reopen closed tab"],
  ["Ctrl+L / Alt+D", "Focus the address bar"],
  ["Ctrl+1 to 8, Ctrl+9", "Go to tab 1 to 8, the last tab"],
  ["Ctrl+PgUp / Ctrl+PgDn", "Previous / next tab"],
  ["Ctrl+Alt+1 to 9", "Switch to space 1 to 9"],
  ["Ctrl+F", "Find in page (Enter or F3 for the next match)"],
  ["Ctrl+H", "History"],
  ["Ctrl+D", "Bookmark this page"],
  ["Ctrl+J", "Downloads"],
  ["Ctrl+Shift+E", "Compare this page with Microsoft Edge"],
  ["Ctrl+Alt+C", "Compact mode"],
  ["Ctrl+,", "Settings"],
  ["Ctrl+R / F5", "Reload"],
  ["Alt+Left / Alt+Right", "Back / forward"],
  ["Ctrl+= / Ctrl+- / Ctrl+0", "Zoom in / out / reset"],
  ["Ctrl+Q", "Quit"],
];
$("shortcuts").replaceChildren(...SHORTCUTS.map(([keys, action]) => {
  const row = document.createElement("tr");
  const key = document.createElement("td");
  for (const [index, part] of keys.split(" / ").entries()) {
    if (index) key.append(" / ");
    const kbd = document.createElement("kbd");
    kbd.textContent = part;
    key.append(kbd);
  }
  const what = document.createElement("td");
  what.textContent = action;
  row.append(key, what);
  return row;
}));

// ---- About ----

$("check-updates").addEventListener("click", () => {
  $("update-status").textContent = "Checking…";
  send("check-updates");
});

// ---- Translation ----

const translation = () => state.translate || {};
$("tr-auto").addEventListener("click", () => send("tr-auto", translation().auto ? "false" : "true"));
$("tr-target").addEventListener("change", () => send("tr-target", $("tr-target").value));
function listRow(text, action, onClick) {
  const row = document.createElement("div");
  row.className = "login never";
  const name = document.createElement("span");
  name.className = "login-host";
  name.textContent = text;
  const button = document.createElement("button");
  button.className = "secondary";
  button.textContent = action;
  button.addEventListener("click", onClick);
  row.append(name, button);
  return row;
}
function renderTranslation() {
  const data = translation();
  pressed($("tr-auto"), data.auto);
  const select = $("tr-target");
  const targets = data.targets || [];
  const key = JSON.stringify(targets);
  if (select.dataset.key !== key) {
    select.dataset.key = key;
    select.replaceChildren(...targets.map((target) => {
      const option = document.createElement("option");
      option.value = target.code;
      option.textContent = target.name;
      return option;
    }));
  }
  if (document.activeElement !== select) select.value = data.to || "en";
  $("tr-catalog-error").hidden = !data.catalogError;
  $("tr-catalog-error").textContent = data.catalogError || "";
  const rows = [
    ...(data.always || []).map((language) => listRow(`Always translate ${language.name}`, "Remove", () => send("tr-unlist", `always:${language.code}`))),
    ...(data.neverLanguages || []).map((language) => listRow(`Never translate ${language.name}`, "Remove", () => send("tr-unlist", `never-language:${language.code}`))),
    ...(data.neverSites || []).map((site) => listRow(`Never translate ${site}`, "Remove", () => send("tr-unlist", `never-site:${site}`))),
  ];
  $("tr-lists").replaceChildren(...rows);
  $("tr-lists-empty").hidden = rows.length > 0;
  const models = data.models || [];
  $("tr-models").replaceChildren(...models.map((model) =>
    listRow(`${model.name} · ${Math.max(1, Math.round(model.size / 1e6))} MB`, "Delete", () => send("tr-delete-model", model.pair))));
  $("tr-models-empty").hidden = models.length > 0;
}
// The list of languages to translate into comes from Mozilla's model list.
send("tr-models");

// ---- Calls from Stillwater ----

window.stillwaterSettings = {
  setState(newState) {
    state = newState;
    renderGeneral();
    renderLogins();
    renderDownloads();
    renderTranslation();
    renderZen();
  },
  // A saved password the user asked to see (sent only on request, never in the state).
  revealPassword(id, password) {
    if (openLogin !== id) return;
    shownPassword = password;
    renderLogins();
    clearTimeout(hideTimer);
    hideTimer = setTimeout(() => { shownPassword = null; renderLogins(); }, 30000);
  },
  histCleared(message) {
    $("clear-status").textContent = message;
  },
};

showPane((location.hash || "#general").slice(1));
send("ready");
