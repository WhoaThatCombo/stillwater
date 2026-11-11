/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

// Address-bar suggestion rows, shared by the sidebar's URL bar (history.js) and the floating
// new-tab address bar (commandbar.js). Rows come from desktop/history.rs:
//   { kind: search | visit | tab | pin | bookmark | history | command, title, url, space?, starred?,
//     command?, argument? }

"use strict";

const SUGGESTION_GLYPHS = {
  search: "", visit: "", tab: "", pin: "", bookmark: "", history: "",
  command: "",
};

function suggestionHost(address) {
  try {
    const parsed = new URL(address);
    return parsed.protocol === "file:" ? parsed.pathname : parsed.host.replace(/^www\./, "");
  } catch (_) {
    return address;
  }
}

function suggestionDetail(row, engine, newTab) {
  switch (row.kind) {
    case "search": return `Search with ${engine}`;
    case "visit": return newTab ? "Open in a new tab" : "Go to address";
    case "tab": return row.space ? `Switch to tab · ${row.space}` : "Switch to tab";
    case "pin": return row.space === "Essentials" ? "Switch to Essential" : "Switch to pinned tab";
    case "command": return "Nothing from it is saved on this device";
    default: return suggestionHost(row.url);
  }
}

// Fill `box` with one element per row. `selected` is the highlighted index (-1 for none);
// `onHover(index)` and `onPick(row)` handle the mouse.
function renderSuggestionRows(box, rows, { engine, newTab = false, onHover, onPick }) {
  box.replaceChildren(...rows.map((row, index) => {
    const item = document.createElement("div");
    item.className = `suggestion ${row.kind}`;
    item.setAttribute("role", "option");
    const icon = document.createElement("span");
    icon.className = "icon suggestion-icon";
    icon.textContent = SUGGESTION_GLYPHS[row.kind] || SUGGESTION_GLYPHS.history;
    const text = document.createElement("span");
    text.className = "suggestion-text";
    const title = document.createElement("span");
    title.className = "suggestion-title";
    title.textContent = row.title || row.url;
    const detail = document.createElement("span");
    detail.className = "suggestion-detail";
    detail.textContent = suggestionDetail(row, engine, newTab);
    text.append(title, detail);
    item.append(icon, text);
    if (row.starred) {
      const star = document.createElement("span");
      star.className = "icon suggestion-star";
      star.textContent = "";
      item.append(star);
    }
    item.title = row.url || "";
    // Keep the focus in the text field so the click lands.
    item.addEventListener("mousedown", (event) => event.preventDefault());
    item.addEventListener("mousemove", () => onHover(index));
    item.addEventListener("click", () => onPick(row));
    return item;
  }));
}

function markSuggestion(box, selected) {
  [...box.children].forEach((row, index) => {
    row.classList.toggle("selected", index === selected);
    row.setAttribute("aria-selected", String(index === selected));
  });
}

// Arrow keys move the selection (wrapping through "nothing selected"). Returns the new index.
function moveSuggestion(selected, count, key) {
  if (key === "ArrowDown") return selected + 1 >= count ? -1 : selected + 1;
  return selected <= -1 ? count - 1 : selected - 1;
}
