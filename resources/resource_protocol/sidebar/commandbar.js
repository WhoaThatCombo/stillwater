/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

// Zen's floating address bar for a new tab. Talks to desktop/command_bar.rs:
//   page -> Rust: console.log("__bar__<command>\t<argument>"): ready, suggest, open, switch,
//                 close, height, debug
//   Rust -> page: window.bar.open({text, engine}) and window.bar.suggestions(request, reply)

"use strict";

const sendBar = (command, argument = "") => console.log(`__bar__${command}\t${argument}`);
const input = document.getElementById("input");
const list = document.getElementById("list");
let rows = [];
let selected = -1;
let request = 0;
let engine = "your search engine";
let lastMs = 0;

function reportHeight() {
  sendBar("height", Math.ceil(document.body.getBoundingClientRect().height));
}

function syncPlaceholder() {
  input.parentElement.classList.toggle("has-value", input.value.length > 0);
}

function render() {
  list.hidden = rows.length === 0;
  renderSuggestionRows(list, rows, {
    engine,
    newTab: true,
    onHover: (index) => {
      if (selected !== index) {
        selected = index;
        markSuggestion(list, selected);
      }
    },
    onPick: pick,
  });
  markSuggestion(list, selected);
  reportHeight();
}

function suggest() {
  syncPlaceholder();
  const text = input.value.trim();
  request++;
  if (!text) {
    rows = [];
    selected = -1;
    render();
  }
  // Also tells Stillwater the text, so it can be offered again if the bar is reopened soon.
  sendBar("suggest", `${request}\t${input.value}`);
}

function pick(row) {
  if (row.kind === "tab" || row.kind === "pin" || row.kind === "command") {
    sendBar("switch", `${row.command}\t${row.argument}`);
  } else if (row.kind === "visit") {
    sendBar("open", `visit\t${input.value.trim()}`);
  } else {
    sendBar("open", `url\t${row.url}`);
  }
}

input.addEventListener("input", suggest);
input.addEventListener("keydown", (event) => {
  if (event.key === "ArrowDown" || event.key === "ArrowUp") {
    selected = moveSuggestion(selected, rows.length, event.key);
    markSuggestion(list, selected);
  } else if (event.key === "Enter") {
    if (selected >= 0 && rows[selected]) pick(rows[selected]);
    else if (input.value.trim()) sendBar("open", `visit\t${input.value.trim()}`);
  } else if (event.key === "Escape") {
    sendBar("close");
  } else {
    return;
  }
  event.preventDefault();
});

window.bar = {
  open(options) {
    engine = options.engine || engine;
    input.value = options.text || "";
    rows = [];
    selected = -1;
    render();
    input.focus();
    if (input.value) {
      input.select();
      suggest();
    }
    syncPlaceholder();
  },
  suggestions(number, reply) {
    if (number !== request) return;
    rows = input.value.trim() ? reply.rows || [] : [];
    lastMs = reply.ms;
    selected = rows.length ? 0 : -1;
    render();
  },
  // For tests.
  state() {
    return { text: input.value, rows: rows.map((row) => [row.kind, row.title, row.url]), selected, ms: lastMs, focused: document.activeElement === input };
  },
};

sendBar("ready");
reportHeight();
