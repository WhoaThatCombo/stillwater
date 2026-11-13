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
  // Rebuilding the buttons would drop keyboard focus; put it back on the same one.
  const focused = [...container.children].indexOf(document.activeElement);
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
  if (focused >= 0 && container.children[focused]) container.children[focused].focus({ preventScroll: true });
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
wireChoices($("preload"), (value) => send("set-preload", value));
wireChoices($("newtab"), (value) => send("set-newtab", value));
wireChoices($("scrollbar"), (value) => send("set-scrollbar", value));
$("smooth-scroll").addEventListener("click", () => send("set-smooth-scroll", state.settings && state.settings.smoothScroll === false ? "1" : "0"));
$("compact").addEventListener("click", () => send("toggle-compact"));
$("lowmem").addEventListener("click", () => send("set-unload-low-memory", state.settings && state.settings.unloadOnLowMemory ? "0" : "1"));
$("splash").addEventListener("click", () => send("set-splash", state.settings && state.settings.startupAnimation === false ? "1" : "0"));
$("spaces").addEventListener("click", () => send("set-spaces", state.settings && state.settings.spaces ? "0" : "1"));
$("adblock").addEventListener("click", () => send("adblock-enabled", state.adblock && state.adblock.enabled ? "0" : "1"));

function renderGeneral() {
  const settings = state.settings || {};
  markChoices($("theme"), settings.theme || "system");
  markChoices($("startup"), settings.startup || "restore");
  markChoices($("preload"), settings.preloadTabs || "all");
  markChoices($("newtab"), settings.newTabBar === false ? "0" : "1");
  markChoices($("scrollbar"), settings.scrollbar || "auto");
  pressed($("smooth-scroll"), settings.smoothScroll !== false);
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
  button.style.minWidth = `${button.offsetWidth}px`;
  button.textContent = text;
  setTimeout(() => { button.textContent = original; button.style.minWidth = ""; }, 1200);
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
  // Rebuilding the rows drops keyboard focus; put it back on the same row and button.
  const held = document.activeElement && document.activeElement.closest ? document.activeElement.closest(".login") : null;
  const heldRow = held && held.parentNode === $("logins") ? [...$("logins").children].indexOf(held) : -1;
  const heldButton = heldRow >= 0 ? [...held.querySelectorAll("button")].indexOf(document.activeElement) : -1;
  $("logins").replaceChildren(...list.map(loginRow));
  const back = heldRow >= 0 && $("logins").children[heldRow] && $("logins").children[heldRow].querySelectorAll("button")[heldButton];
  if (back) back.focus({ preventScroll: true });
  $("logins-empty").hidden = list.length > 0;
  $("logins-empty").textContent = query
    ? "No saved passwords match."
    : "No saved passwords yet. Stillwater offers to save one when you sign in to a site, or you can import them from another browser (Import).";
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

// ---- Import from another browser ----
// Zen, Firefox and its relatives, Chrome, Edge, Opera (GX), Brave, Vivaldi: the browsers
// Stillwater finds on this computer (browser_import.rs). Zen's import is the same as ever.

const importKinds = ["pins", "tabs", "passwords", "bookmarks", "history", "cookies"];
const importChoice = { browser: "", profile: 0, pins: true, tabs: false, passwords: true, bookmarks: true, history: false, cookies: true };
let importBusy = false;
let importing = false;
const imported = () => state.browserImport || null;
// While a scan is in flight the list keeps its rows (and the last list of browsers) and the
// counts say "Counting…", so nothing above or below moves; the answer replaces them in place.
let scanning = null;
let scanTimer = 0;
function scanBrowser(browser, profile) {
  importBusy = true;
  importChoice.browser = browser;
  importChoice.profile = profile;
  const last = imported() || {};
  const known = (last.browsers || []).find((item) => item.id === browser);
  scanning = { browsers: last.browsers, browser, profile, name: known ? known.name : "", hasTabs: last.hasTabs, spacesKind: last.spacesKind, scanning: true };
  clearTimeout(scanTimer);
  scanTimer = setTimeout(() => { scanning = null; renderImport(); }, 15000);
  renderImport();
  send("import-scan", `${browser}\t${profile}`);
}
$("import-scan").addEventListener("click", () => scanBrowser(importChoice.browser, importChoice.profile));
document.querySelectorAll("[data-import]").forEach((toggle) => toggle.addEventListener("click", () => {
  importChoice[toggle.dataset.import] = !importChoice[toggle.dataset.import];
  renderImport();
}));
// What the chosen browser can give.
function importable(data) {
  return importKinds.filter((what) => (what !== "pins" && what !== "tabs") || data.hasTabs);
}
$("import-run").addEventListener("click", () => {
  const data = imported() || {};
  const what = importable(data).filter((name) => importChoice[name]);
  if (what.length === 0 || !data.browser) return;
  importing = true;
  $("import-status").textContent = `Importing from ${data.name}…`;
  send("import-run", `${data.browser}\t${data.profile || 0}\t${what.join(",")}`);
});
$("import-csv").addEventListener("click", () => send("import-passwords-file"));
function renderImport() {
  let data = imported();
  const fileResult = data && data.fileResult;
  if (scanning) {
    const answered = data && data.browsers && (!scanning.browser || (data.browser === scanning.browser && (data.profile || 0) === scanning.profile));
    if (answered) { scanning = null; clearTimeout(scanTimer); } else data = scanning;
  }
  $("import-csv-status").textContent = fileResult || "";
  if (!data || !data.browsers) {
    $("import-found").hidden = true;
    $("import-status").textContent = importBusy ? "Looking for browsers…" : "";
    return;
  }
  importBusy = false;
  const browsers = data.browsers;
  if (browsers.length === 0 || !data.browser) {
    $("import-found").hidden = true;
    $("import-status").textContent = "No other browser's data was found on this computer (Chrome, Edge, Firefox, Zen, Opera, Opera GX, Brave, Vivaldi, LibreWolf, Floorp or Waterfox).";
    return;
  }
  $("import-found").hidden = false;
  importChoice.browser = data.browser;
  importChoice.profile = data.profile || 0;
  choices($("import-browsers"), browsers.map((browser) => ({
    label: browser.name,
    detail: browser.profiles.length > 1 ? `${browser.profiles.length} profiles` : "",
    value: browser.id,
  })), data.browser, (id) => { if (id !== data.browser) scanBrowser(id, 0); });
  const current = browsers.find((browser) => browser.id === data.browser) || { profiles: [] };
  // Reserve room for the longest profile list of any browser, so switching browsers keeps the
  // rows below where they are.
  const slots = Math.max(0, ...browsers.map((browser) => browser.profiles.length));
  $("import-profiles").style.minHeight = slots < 2 ? "" : `${slots * 39 + (slots - 1) * 4}px`;
  $("import-profiles").style.visibility = current.profiles.length < 2 ? "hidden" : "";
  choices($("import-profiles"), current.profiles.map((name, index) => ({ label: name, value: index })), importChoice.profile,
    (index) => scanBrowser(data.browser, index));
  const zen = data.spacesKind === "zen";
  document.querySelector("[data-import-label=pins]").textContent = zen ? "Spaces and pinned tabs" : "Pinned tabs";
  const protectedNote = (n) => n ? `; ${count(n, "more is", "more are")} protected by ${data.name} and can't be imported` : "";
  const counts = {
    pins: data.sessionProblem || (zen
      ? `${count(data.spaces, "space", "spaces")}, ${count(data.pins, "pinned tab", "pinned tabs")}, ${count(data.essentials, "Essential", "Essentials")}`
      : count(data.pins, "pinned tab", "pinned tabs")),
    tabs: data.sessionProblem ? "" : `${count(data.tabs, "tab", "tabs")}, added unloaded`,
    passwords: data.passwordProblem || count(data.passwords, "password", "passwords") + protectedNote(data.protectedPasswords),
    bookmarks: data.placesProblem || count(data.bookmarks || 0, "bookmark", "bookmarks"),
    history: data.placesProblem || `${count(data.visits || 0, "visit", "visits")} in the last 90 days`,
    cookies: data.cookiesProblem || `${count(data.cookies || 0, "cookie", "cookies")}; replaces Stillwater's cookies for the same sites` + protectedNote(data.protectedCookies),
  };
  const offered = importable(data);
  // Rows a browser can't give stay in place, greyed out, so the rest doesn't shift.
  const missing = () => `Not available from ${data.name || "this browser"}`;
  document.querySelectorAll("[data-import-count]").forEach((span) => {
    const what = span.dataset.importCount;
    const text = data.scanning ? "Counting…" : offered.includes(what) ? counts[what] : missing();
    span.textContent = text;
    span.title = text;
  });
  document.querySelectorAll("[data-import]").forEach((toggle) => {
    toggle.disabled = !offered.includes(toggle.dataset.import);
    pressed(toggle, importChoice[toggle.dataset.import] && !toggle.disabled);
  });
  $("import-run").disabled = !!data.scanning || !offered.some((what) => importChoice[what]);
  if (data.result) importing = false;
  $("import-status").textContent = data.scanning ? "" : data.result || (importing ? `Importing from ${data.name}…` : "");
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
    renderImport();
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
