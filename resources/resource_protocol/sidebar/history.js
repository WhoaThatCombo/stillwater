/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

// History, address-bar suggestions and bookmarks (desktop/history.rs, desktop/bookmarks.rs).
// Loaded after sidebar.js and uses its globals (send, $, state, url, urlbar, openMenu…).
//   page -> Rust: hist-suggest, hist-warm, hist-list, hist-delete, hist-clear,
//                 bm-star, bm-rename, bm-delete, bm-list
//   Rust -> page: window.sidebar.histSuggestions / histList / histCleared / bmEdit / bmList /
//                 openHistory, and state.hist ({canBookmark, starred}) for the star.

"use strict";

const hostOf = suggestionHost;

function engineName() {
  const settings = state.settings || {};
  const engine = (settings.searchEngines || []).find((item) => item.id === settings.searchEngine);
  return engine ? engine.name : "your search engine";
}

// ---- Address-bar suggestions ----

const suggestionBox = $("suggestions");
let suggestionRows = [];
let selectedSuggestion = -1;
let suggestRequest = 0;

function closeSuggestions() {
  suggestionBox.hidden = true;
  suggestionBox.replaceChildren();
  suggestionRows = [];
  selectedSuggestion = -1;
  suggestRequest++;
}

function markSelected() {
  markSuggestion(suggestionBox, selectedSuggestion);
}

function renderSuggestions() {
  if (suggestionRows.length === 0 || document.activeElement !== url) {
    closeSuggestions();
    return;
  }
  renderSuggestionRows(suggestionBox, suggestionRows, {
    engine: engineName(),
    onHover: (index) => {
      if (selectedSuggestion !== index) {
        selectedSuggestion = index;
        markSelected();
      }
    },
    onPick: pickSuggestion,
  });
  const box = urlbar.getBoundingClientRect();
  suggestionBox.style.left = `${box.left}px`;
  suggestionBox.style.width = `${box.width}px`;
  suggestionBox.style.top = `${box.bottom + 4}px`;
  suggestionBox.hidden = false;
  markSelected();
}

function pickSuggestion(row) {
  const typed = url.value.trim();
  closeSuggestions();
  if (row.kind === "tab" || row.kind === "pin") {
    send(row.command, row.argument);
    url.value = state.url === "servo:newtab" ? "" : state.url;
    url.blur();
    send("blur");
  } else {
    // "Go to" does exactly what Enter does with the typed text.
    send("go", row.kind === "visit" ? typed : row.url);
    url.blur();
  }
}

url.addEventListener("input", () => {
  const text = url.value.trim();
  if (!text) {
    closeSuggestions();
    return;
  }
  send("hist-suggest", `${++suggestRequest}\t${text}`);
});
url.addEventListener("focus", () => send("hist-warm"));
url.addEventListener("blur", closeSuggestions);

// Runs before sidebar.js's own URL bar keys (capture phase), so a selected row wins over
// "go to the typed text" and Escape first just closes the list.
document.addEventListener("keydown", (event) => {
  if (event.target !== url || suggestionBox.hidden) return;
  const count = suggestionRows.length;
  let handled = true;
  if (event.key === "ArrowDown" || event.key === "ArrowUp") {
    selectedSuggestion = moveSuggestion(selectedSuggestion, count, event.key);
    markSelected();
  } else if (event.key === "Enter" && selectedSuggestion >= 0) {
    pickSuggestion(suggestionRows[selectedSuggestion]);
  } else if (event.key === "Escape") {
    closeSuggestions();
  } else {
    handled = false;
  }
  if (handled) {
    event.preventDefault();
    event.stopImmediatePropagation();
  }
}, true);

// ---- The star: bookmark the current page ----

const star = $("star");
const editor = $("bm-editor");
const editorName = $("bm-editor-name");
let editing = null;

function renderStar() {
  const hist = state.hist || {};
  star.hidden = !hist.canBookmark;
  star.classList.toggle("starred", !!hist.starred);
  star.textContent = hist.starred ? "" : "";
  star.title = hist.starred ? "Edit this bookmark (Ctrl+D)" : "Bookmark this page (Ctrl+D)";
}

function closeEditor(save) {
  if (!editing) return;
  const name = editorName.value.trim();
  if (save && name && name !== editing.title) send("bm-rename", `${editing.id}\t${name}`);
  editing = null;
  editor.hidden = true;
}

star.addEventListener("click", (event) => {
  event.stopPropagation();
  if (editing) closeEditor(true);
  else send("bm-star");
});
$("bm-editor-done").addEventListener("click", () => closeEditor(true));
$("bm-editor-remove").addEventListener("click", () => {
  if (editing) send("bm-delete", editing.id);
  closeEditor(false);
});
editorName.addEventListener("keydown", (event) => {
  if (event.key === "Enter") closeEditor(true);
  else if (event.key === "Escape") closeEditor(false);
});
document.addEventListener("mousedown", (event) => {
  if (editing && !editor.contains(event.target) && event.target !== star) closeEditor(true);
});

// ---- History panel ----

const historyPanel = $("history");
const historySearch = $("history-search");
let historyView = "history";
let historyRows = [];
let historyMore = false;
let bookmarkRows = [];
let searchTimer = 0;

const MONTHS = ["January", "February", "March", "April", "May", "June", "July", "August",
  "September", "October", "November", "December"];
const WEEKDAYS = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];
const pad = (number) => String(number).padStart(2, "0");

function dayLabel(time) {
  const date = new Date(time);
  const today = new Date();
  today.setHours(0, 0, 0, 0);
  const day = new Date(date);
  day.setHours(0, 0, 0, 0);
  const days = Math.round((today - day) / 86400000);
  if (days === 0) return "Today";
  if (days === 1) return "Yesterday";
  const label = `${WEEKDAYS[date.getDay()]}, ${date.getDate()} ${MONTHS[date.getMonth()]}`;
  return date.getFullYear() === today.getFullYear() ? label : `${label} ${date.getFullYear()}`;
}

function entryRow(entry, detail, onDelete, deleteTitle, menu) {
  const row = document.createElement("div");
  row.className = "hist-row";
  row.title = entry.url;
  const icon = tabIcon({ title: entry.title || hostOf(entry.url), url: entry.url });
  const text = document.createElement("span");
  text.className = "hist-text";
  const title = document.createElement("span");
  title.className = "hist-title";
  title.textContent = entry.title || entry.url;
  const sub = document.createElement("span");
  sub.className = "hist-detail";
  sub.textContent = detail;
  text.append(title, sub);
  const remove = iconButton("hist-delete", "", deleteTitle, onDelete);
  row.append(icon, text, remove);
  row.addEventListener("click", () => send("go", entry.url));
  attachMenu(row, menu);
  return row;
}

function renderHistoryList() {
  const list = $("history-list");
  const empty = $("history-empty");
  const query = historySearch.value.trim();
  if (historyView === "history") {
    const rows = [];
    let day = null;
    for (const entry of historyRows) {
      const label = dayLabel(entry.time);
      if (label !== day) {
        day = label;
        const header = document.createElement("h3");
        header.className = "hist-day";
        header.textContent = label;
        rows.push(header);
      }
      const time = new Date(entry.time);
      const remove = () => {
        send("hist-delete", entry.id);
        historyRows = historyRows.filter((other) => other.id !== entry.id);
        renderHistoryList();
      };
      rows.push(entryRow(entry, `${pad(time.getHours())}:${pad(time.getMinutes())} · ${hostOf(entry.url)}`,
        remove, "Delete from history", () => [
          { label: "Open", run: () => send("go", entry.url) },
          { label: "Delete from history", run: remove },
        ]));
    }
    list.replaceChildren(...rows);
    empty.hidden = historyRows.length > 0;
    empty.textContent = query ? "No pages in your history match." : "Pages you visit will be listed here.";
    $("history-more").hidden = !historyMore;
    $("history-clear-group").hidden = false;
  } else {
    const words = query.toLowerCase().split(/\s+/).filter(Boolean);
    const matches = bookmarkRows.filter((bookmark) => {
      const haystack = `${bookmark.title} ${bookmark.url}`.toLowerCase();
      return words.every((word) => haystack.includes(word));
    });
    list.replaceChildren(...matches.slice(0, 500).map((bookmark) => {
      const remove = () => {
        send("bm-delete", bookmark.id);
        bookmarkRows = bookmarkRows.filter((other) => other.id !== bookmark.id);
        renderHistoryList();
      };
      const detail = bookmark.folder ? `${hostOf(bookmark.url)} · ${bookmark.folder}` : hostOf(bookmark.url);
      const row = entryRow(bookmark, detail, remove, "Delete bookmark", () => [
        { label: "Open", run: () => send("go", bookmark.url) },
        { label: "Rename", run: () => startRename(row.querySelector(".hist-title"), bookmark.title, (title) => {
          if (!title) return;
          bookmark.title = title;
          send("bm-rename", `${bookmark.id}\t${title}`);
          renderHistoryList();
        }) },
        "-",
        { label: "Delete bookmark", run: remove },
      ]);
      return row;
    }));
    empty.hidden = matches.length > 0;
    empty.textContent = query
      ? "No bookmarks match."
      : "No bookmarks yet. Click the star in the address bar or press Ctrl+D to bookmark a page, or import them from Zen in settings.";
    $("history-more").hidden = true;
    $("history-clear-group").hidden = true;
  }
}

function requestHistory(append) {
  if (historyView === "history") {
    const before = append && historyRows.length ? historyRows[historyRows.length - 1].time : 0;
    send("hist-list", `${before}\t${historySearch.value.trim()}`);
  } else {
    send("bm-list");
  }
}

function showHistoryView(view) {
  historyView = view;
  $("history-heading").textContent = view === "history" ? "History" : "Bookmarks";
  $("history-search-placeholder").textContent = view === "history" ? "Search history" : "Search bookmarks";
  document.querySelectorAll("#history-views button").forEach((button) => {
    button.setAttribute("aria-pressed", String(button.dataset.view === view));
  });
  closeClearOptions();
  renderHistoryList();
  requestHistory(false);
}

function openHistory(view = "history") {
  historyPanel.hidden = false;
  historySearch.value = "";
  historySearch.parentElement.classList.remove("has-value");
  $("clear-status").hidden = true;
  showHistoryView(view);
}

function closeHistory() {
  historyPanel.hidden = true;
  closeClearOptions();
}

// Clearing: pick a range, optionally cookies and site data too, then confirm.
let clearRange = "hour";
let clearSiteData = false;

function renderClearOptions() {
  document.querySelectorAll("#clear-range button").forEach((button) => {
    button.setAttribute("aria-pressed", String(button.dataset.range === clearRange));
  });
  $("clear-site-data").setAttribute("aria-pressed", String(clearSiteData));
}

function closeClearOptions() {
  $("clear-options").hidden = true;
  $("history-clear").hidden = false;
}

function clearSince(range) {
  if (range === "hour") return Date.now() - 3600 * 1000;
  if (range === "today") {
    const midnight = new Date();
    midnight.setHours(0, 0, 0, 0);
    return midnight.getTime();
  }
  return 0;
}

$("history-clear").addEventListener("click", () => {
  clearRange = "hour";
  clearSiteData = false;
  renderClearOptions();
  $("clear-options").hidden = false;
  $("history-clear").hidden = true;
  $("clear-status").hidden = true;
});
document.querySelectorAll("#clear-range button").forEach((button) => {
  button.addEventListener("click", () => {
    clearRange = button.dataset.range;
    renderClearOptions();
  });
});
$("clear-site-data").addEventListener("click", () => {
  clearSiteData = !clearSiteData;
  renderClearOptions();
});
$("clear-cancel").addEventListener("click", closeClearOptions);
$("clear-confirm").addEventListener("click", () => {
  send("hist-clear", `${clearSince(clearRange)}\t${clearSiteData ? 1 : 0}`);
  closeClearOptions();
});

$("history-close").addEventListener("click", closeHistory);
$("history-more").addEventListener("click", () => requestHistory(true));
document.querySelectorAll("#history-views button").forEach((button) => {
  button.addEventListener("click", () => showHistoryView(button.dataset.view));
});
historySearch.addEventListener("input", () => {
  historySearch.parentElement.classList.toggle("has-value", historySearch.value.length > 0);
  if (historyView === "bookmarks") {
    renderHistoryList();
    return;
  }
  clearTimeout(searchTimer);
  searchTimer = setTimeout(() => requestHistory(false), 150);
});
document.addEventListener("keydown", (event) => {
  if (event.key === "Escape" && !historyPanel.hidden && event.target !== url) closeHistory();
});
$("open-history").addEventListener("click", () => openHistory("history"));
$("open-bookmarks").addEventListener("click", () => openHistory("bookmarks"));

// ---- Calls from Rust ----

Object.assign(window.sidebar, {
  histSuggestions(request, reply) {
    if (request !== suggestRequest) return; // an answer to an older keystroke
    suggestionRows = reply.rows || [];
    selectedSuggestion = -1;
    lastSuggestMs = reply.ms;
    renderSuggestions();
  },
  histList(reply) {
    historyRows = reply.append ? historyRows.concat(reply.rows) : reply.rows;
    historyMore = reply.more;
    if (!historyPanel.hidden && historyView === "history") renderHistoryList();
  },
  histCleared(message) {
    const status = $("clear-status");
    status.textContent = message;
    status.hidden = false;
    if (!historyPanel.hidden) requestHistory(false);
  },
  bmList(list) {
    bookmarkRows = list;
    if (!historyPanel.hidden && historyView === "bookmarks") renderHistoryList();
  },
  bmEdit(bookmark) {
    if (!bookmark || bookmark.id === undefined) return;
    editing = bookmark;
    $("bm-editor-title").textContent = state.hist && state.hist.starred ? "Edit bookmark" : "Bookmark added";
    editorName.value = bookmark.title;
    editor.style.top = `${urlbar.getBoundingClientRect().bottom + 4}px`;
    editor.hidden = false;
    editorName.focus();
    editorName.select();
  },
  openHistory() {
    openHistory("history");
    historySearch.focus();
  },
});

// For tests: how long the last suggestion query took in Rust, in milliseconds.
let lastSuggestMs = 0;

// ---- New tab: Zen's floating address bar (desktop/command_bar.rs, commandbar.html) ----

// While the bar is open no tab shows as selected and the New Tab row does, like Zen.
function renderCommandBar() {
  const open = !!state.commandBar;
  $("new-tab").classList.toggle("selected", open);
  if (open) {
    document.querySelectorAll("#tabs .tab.selected, #pinned .tab.selected, #essentials .essential.selected")
      .forEach((row) => row.classList.remove("selected"));
  }
  const bar = !state.settings || state.settings.newTabBar !== false;
  document.querySelectorAll("#newtab-choice button").forEach((button) => {
    button.setAttribute("aria-pressed", String((button.dataset.bar === "1") === bar));
  });
}
document.querySelectorAll("#newtab-choice button").forEach((button) => {
  button.addEventListener("click", () => send("set-newtab", button.dataset.bar));
});

// Re-render the star and the New Tab row with every state update.
{
  const setState = window.sidebar.setState;
  window.sidebar.setState = (newState) => {
    setState(newState);
    renderStar();
    renderCommandBar();
  };
}
renderStar();
