/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

// Tabs surviving and moving (desktop/sidebar/session.rs): the "On startup" setting, and
// drag and drop in the sidebar. Loaded after sidebar.js and uses its globals.
//
// Drag and drop uses pointer events rather than the HTML drag-and-drop API: a press on a
// tab, pinned tab or Essential becomes a drag once the pointer moves a few pixels (so clicks
// still work). A line shows where the tab will go; folders, Essentials and space buttons
// light up when the tab would go into them.

"use strict";

// ---- "On startup" setting ----

function renderSessionSettings() {
  const startup = (state.settings && state.settings.startup) || "restore";
  document.querySelectorAll("#session-startup button").forEach((button) => {
    button.setAttribute("aria-pressed", String(button.dataset.startup === startup));
  });
}
document.querySelectorAll("#session-startup button").forEach((button) => {
  button.addEventListener("click", () => send("session-startup", button.dataset.startup));
});

// ---- Drag and drop ----

const DRAG_THRESHOLD = 5;
// The press that may become a drag: { source, startX, startY, active, target, ghost }.
let drag = null;
let suppressClick = false;

const dropLine = document.createElement("div");
dropLine.id = "session-drop-line";
dropLine.hidden = true;
document.body.append(dropLine);

// What is being dragged: an ordinary tab, a pinned tab or an Essential.
function dragSourceAt(element) {
  if (!element || element.closest(".tab-close, .tab-more, .tab-reset, input, #menu")) return null;
  const row = element.closest("[data-tab], [data-pin]");
  if (!row) return null;
  if (row.dataset.tab) return { kind: "tab", id: row.dataset.tab };
  return { kind: "pin", id: row.dataset.pin, essential: row.dataset.essential === "1" };
}

// The row for the source, looked up again because the lists are rebuilt on every update.
function sourceElement() {
  const { kind, id } = drag.source;
  return document.querySelector(kind === "tab" ? `#tabs [data-tab="${id}"]` : `[data-pin="${id}"]`);
}

// The next row after `row` that is a sibling of the same kind and group.
function nextRow(row, matches) {
  for (let next = row.nextElementSibling; next; next = next.nextElementSibling) {
    if (matches(next)) return next;
    if (next.dataset.folderId !== undefined) return null; // a folder starts: end of the group
  }
  return null;
}

// Where the pointer would drop: { type: "tabs" | "pins" | "space", ... } or null.
function dropTargetAt(x, y) {
  const element = document.elementFromPoint(x, y);
  if (!element || element.closest("#menu, #settings, #passwords")) return null;
  const source = drag.source;

  const space = element.closest("[data-space]");
  if (space) {
    if (Number(space.dataset.space) === state.workspaces.current) return null;
    return { type: "space", space: space.dataset.space, into: space };
  }

  const grid = element.closest("#essentials");
  if (grid) {
    const tile = element.closest("[data-pin]");
    let before = "";
    if (tile) {
      const box = tile.getBoundingClientRect();
      const next = x > box.left + box.width / 2 ? tile.nextElementSibling : tile;
      before = next ? next.dataset.pin : "";
    }
    return { type: "pins", essential: true, folder: "", before, into: grid };
  }

  const folder = element.closest("[data-folder-id]");
  if (folder) {
    return { type: "pins", essential: false, folder: folder.dataset.folderId, before: "", into: folder };
  }

  const row = element.closest("[data-pin], [data-tab]");
  if (row) {
    const box = row.getBoundingClientRect();
    const after = y > box.top + box.height / 2;
    if (row.dataset.pin) {
      const group = row.dataset.folder;
      const next = after ? nextRow(row, (other) => other.dataset.pin && other.dataset.folder === group) : row;
      return { type: "pins", essential: false, folder: group, before: next ? next.dataset.pin : "", line: { box, y: after ? box.bottom : box.top } };
    }
    const next = after ? nextRow(row, (other) => other.dataset.tab) : row;
    return { type: "tabs", before: next ? next.dataset.tab : "", line: { box, y: after ? box.bottom : box.top } };
  }

  if (element.closest("#pinned, #pinned-separator")) {
    const pinned = $("pinned");
    const box = pinned.getBoundingClientRect();
    const zone = pinned.querySelector(".session-pin-dropzone");
    if (zone) return { type: "pins", essential: false, folder: "", before: "", into: zone };
    return { type: "pins", essential: false, folder: "", before: "", line: { box, y: box.bottom } };
  }

  if (element.closest("#tab-area")) {
    // The New Tab button: before the first tab. Empty space below the list: at the end.
    const first = $("tabs").querySelector("[data-tab]");
    if (element.closest("#new-tab")) {
      const box = (first || $("new-tab")).getBoundingClientRect();
      return { type: "tabs", before: first ? first.dataset.tab : "", line: { box, y: first ? box.top : box.bottom } };
    }
    const rows = $("tabs").querySelectorAll("[data-tab]");
    const last = rows[rows.length - 1];
    const box = (last || $("new-tab")).getBoundingClientRect();
    return { type: "tabs", before: "", line: { box, y: box.bottom } };
  }
  return null;
}

// Dropping a tab where it already is does nothing.
function isNoOp(source, target) {
  if (!target) return true;
  if (target.type === "tabs") return source.kind === "tab" && target.before === source.id;
  if (target.type === "pins") return source.kind === "pin" && target.before === source.id;
  return false;
}

// Show the drop line or highlight, the dimmed source row and the pinned drop zone. Called
// after every move and every re-render (which replaces the rows).
function decorateDrag() {
  document.querySelectorAll(".session-drop-into").forEach((element) => element.classList.remove("session-drop-into"));
  document.querySelectorAll(".session-dragging").forEach((element) => element.classList.remove("session-dragging"));
  const source = sourceElement();
  if (source) source.classList.add("session-dragging");
  // Somewhere to drop a tab to pin it when this space has no pinned tabs yet.
  const pinned = $("pinned");
  if (drag.source.kind === "tab" && !pinned.querySelector("[data-pin], [data-folder-id], .session-pin-dropzone")) {
    const zone = document.createElement("div");
    zone.className = "session-pin-dropzone";
    zone.textContent = "Drop here to pin";
    pinned.append(zone);
  }
  const target = drag.target;
  dropLine.hidden = true;
  if (!target || isNoOp(drag.source, target)) return;
  if (target.into) {
    // Rows may have been rebuilt: find the element again by what it is.
    let into = target.into;
    if (!into.isConnected) {
      if (target.type === "space") into = document.querySelector(`[data-space="${target.space}"]`);
      else if (target.folder) into = document.querySelector(`[data-folder-id="${target.folder}"]`);
      else if (target.essential) into = $("essentials");
      else into = pinned.querySelector(".session-pin-dropzone");
    }
    if (into) into.classList.add("session-drop-into");
  } else if (target.line) {
    const { box, y } = target.line;
    dropLine.style.left = `${box.left + 4}px`;
    dropLine.style.width = `${Math.max(0, box.width - 8)}px`;
    dropLine.style.top = `${Math.round(y) - 1}px`;
    dropLine.hidden = false;
  }
}

function startDrag() {
  drag.active = true;
  closeMenu();
  document.body.classList.add("session-drag");
  const row = sourceElement();
  const ghost = document.createElement("div");
  ghost.className = "session-ghost";
  const label = row && (row.querySelector(".tab-label") || row);
  ghost.textContent = (label && (label.textContent || row.title)) || "";
  document.body.append(ghost);
  drag.ghost = ghost;
}

// Scroll the tab list when dragging near its top or bottom edge.
function autoScroll(y) {
  const area = $("tab-area");
  const box = area.getBoundingClientRect();
  if (y < box.top + 24) area.scrollTop -= 8;
  else if (y > box.bottom - 24 && y < box.bottom + 4) area.scrollTop += 8;
}

function moveDrag(x, y) {
  drag.ghost.style.left = `${x + 12}px`;
  drag.ghost.style.top = `${y + 8}px`;
  autoScroll(y);
  drag.target = dropTargetAt(x, y);
  decorateDrag();
}

function dropCommand(source, target) {
  const pinSlot = (target) => `${target.essential ? "e" : "p"}\t${target.folder || ""}\t${target.before || ""}`;
  if (source.kind === "tab") {
    if (target.type === "tabs") return ["session-move-tab", `${source.id}\t${target.before}`];
    if (target.type === "pins") return ["session-pin-tab", `${source.id}\t${pinSlot(target)}`];
    return ["session-tab-space", `${source.id}\t${target.space}`];
  }
  if (target.type === "tabs") return ["session-unpin-tab", `${source.id}\t${target.before}`];
  if (target.type === "pins") return ["session-place-pin", `${source.id}\t${pinSlot(target)}`];
  return ["session-pin-space", `${source.id}\t${target.space}`];
}

function endDrag(drop) {
  if (!drag) return;
  const finished = drag;
  drag = null;
  if (!finished.active) return;
  dropLine.hidden = true;
  if (finished.ghost) finished.ghost.remove();
  document.body.classList.remove("session-drag");
  document.querySelectorAll(".session-drop-into, .session-dragging").forEach((element) => {
    element.classList.remove("session-drop-into", "session-dragging");
  });
  document.querySelectorAll(".session-pin-dropzone").forEach((zone) => zone.remove());
  // The press ends with a click on whatever is under the pointer: don't let it activate.
  suppressClick = true;
  setTimeout(() => { suppressClick = false; }, 0);
  const target = finished.target;
  if (!drop || !target || isNoOp(finished.source, target)) return;
  const [command, argument] = dropCommand(finished.source, target);
  send(command, argument);
}

document.addEventListener("pointerdown", (event) => {
  if (drag) endDrag(false);
  if (event.button !== 0 || document.querySelector(".rename-input")) return;
  const source = dragSourceAt(event.target);
  if (!source) return;
  drag = {
    source,
    startX: event.clientX,
    startY: event.clientY,
    active: false,
    target: null,
    // Whether this platform reports pressed buttons on moves (to notice a release outside).
    buttonsReported: event.buttons !== 0,
  };
});

document.addEventListener("pointermove", (event) => {
  if (!drag) return;
  if (drag.buttonsReported && event.buttons === 0) {
    endDrag(false); // released outside the sidebar
    return;
  }
  if (!drag.active) {
    if (Math.hypot(event.clientX - drag.startX, event.clientY - drag.startY) < DRAG_THRESHOLD) return;
    startDrag();
  }
  moveDrag(event.clientX, event.clientY);
});

document.addEventListener("pointerup", (event) => {
  if (!drag) return;
  if (drag.active) moveDrag(event.clientX, event.clientY);
  endDrag(true);
});

document.addEventListener("pointercancel", () => endDrag(false));

document.addEventListener("keydown", (event) => {
  if (event.key === "Escape" && drag) endDrag(false);
});

document.addEventListener("click", (event) => {
  if (!suppressClick) return;
  suppressClick = false;
  event.stopPropagation();
  event.preventDefault();
}, true);

// Re-render hooks: keep the setting and the drag decorations up to date.
const sessionBaseSetState = window.sidebar.setState;
window.sidebar.setState = function (newState) {
  sessionBaseSetState.call(this, newState);
  renderSessionSettings();
  if (drag && drag.active) decorateDrag();
};
renderSessionSettings();
