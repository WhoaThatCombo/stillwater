/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

// The downloads indicator in the bottom row and the Downloads panel.
// State: state.downloads (desktop/downloads.rs, Downloads::to_json).
// Commands: dl-cancel, dl-retry, dl-open, dl-show, dl-remove, dl-clear, dl-ask, dl-refresh,
// dl-open-folder.

"use strict";

(() => {
  const panel = $("downloads");
  const button = $("dl-button");
  // The download whose "this can run programs" warning is showing.
  let confirmOpen = null;

  function downloads() {
    return state.downloads || { items: [], active: 0, progress: null, askWhereToSave: false };
  }

  function formatBytes(bytes) {
    if (bytes < 1024) return `${bytes} B`;
    const units = ["KB", "MB", "GB", "TB"];
    let value = bytes;
    let unit = -1;
    do {
      value /= 1024;
      unit += 1;
    } while (value >= 1024 && unit < units.length - 1);
    return `${value >= 100 ? value.toFixed(0) : value.toFixed(1)} ${units[unit]}`;
  }

  function host(url) {
    try {
      const parsed = new URL(url);
      return parsed.host || parsed.protocol.replace(":", "");
    } catch (_) {
      return "";
    }
  }

  function detail(item) {
    switch (item.state) {
      case "downloading": {
        if (item.choosing) return "Choose where to save it…";
        const parts = [item.total ? `${formatBytes(item.received)} of ${formatBytes(item.total)}` : formatBytes(item.received)];
        if (item.speed > 0) parts.push(`${formatBytes(item.speed)}/s`);
        return parts.join(" · ");
      }
      case "done":
        if (item.missing) return "Moved or deleted";
        return [formatBytes(item.received), host(item.url)].filter(Boolean).join(" · ");
      case "failed":
        return item.error ? `Failed. ${item.error}` : "Failed";
      default:
        return "Cancelled";
    }
  }

  function glyph(item) {
    switch (item.state) {
      case "downloading": return "";
      case "done": return "";
      case "failed": return "";
      default: return "";
    }
  }

  function smallButton(label, onClick, className = "") {
    const element = document.createElement("button");
    element.className = `secondary small ${className}`.trim();
    element.textContent = label;
    element.addEventListener("click", (event) => {
      event.stopPropagation();
      onClick();
    });
    return element;
  }

  function renderItem(item) {
    const row = document.createElement("div");
    row.className = `dl-item ${item.state}` + (item.missing ? " missing" : "");
    row.dataset.key = item.key;
    row.title = item.path;

    const icon = document.createElement("span");
    icon.className = "icon dl-item-icon";
    icon.textContent = glyph(item);

    const text = document.createElement("div");
    text.className = "dl-item-text";
    const name = document.createElement("span");
    name.className = "dl-name";
    name.textContent = item.name;
    const info = document.createElement("span");
    info.className = "dl-detail";
    info.textContent = detail(item);
    text.append(name, info);

    if (item.state === "downloading") {
      const bar = document.createElement("div");
      bar.className = "dl-bar" + (item.total ? "" : " indeterminate");
      const fill = document.createElement("div");
      fill.className = "dl-bar-fill";
      if (item.total) fill.style.width = `${Math.min(100, (100 * item.received) / item.total)}%`;
      bar.append(fill);
      text.append(bar);
    }

    const actions = document.createElement("div");
    actions.className = "dl-actions";
    if (confirmOpen === item.key) {
      const warning = document.createElement("p");
      warning.className = "dl-warning";
      warning.textContent = "This file can run programs on your computer. Open it only if you trust where it came from.";
      actions.append(
        warning,
        smallButton("Open", () => { confirmOpen = null; send("dl-open", item.key); render(); }, "danger"),
        smallButton("Cancel", () => { confirmOpen = null; render(); }),
      );
    } else if (item.state === "downloading") {
      actions.append(smallButton("Cancel", () => send("dl-cancel", item.key)));
    } else if (item.state === "done" && !item.missing) {
      actions.append(
        smallButton("Open", () => {
          if (item.executable) {
            confirmOpen = item.key;
            render();
          } else {
            send("dl-open", item.key);
          }
        }),
        smallButton("Show in folder", () => send("dl-show", item.key)),
        smallButton("Remove from list", () => send("dl-remove", item.key)),
      );
    } else {
      if (item.state !== "done" && item.url) actions.append(smallButton("Retry", () => send("dl-retry", item.key)));
      actions.append(smallButton("Remove from list", () => send("dl-remove", item.key)));
    }
    text.append(actions);

    row.append(icon, text);
    return row;
  }

  // The bottom-row button: a ring that fills as downloads progress, and how many are running.
  function renderButton() {
    const data = downloads();
    const active = data.active || 0;
    button.classList.toggle("active", active > 0);
    button.classList.toggle("indeterminate", active > 0 && data.progress === null);
    const fill = button.querySelector(".dl-ring-fill");
    const circumference = 2 * Math.PI * 11.5;
    fill.style.strokeDasharray = `${circumference}`;
    const progress = active > 0 ? (data.progress === null ? 0.25 : data.progress) : 0;
    fill.style.strokeDashoffset = `${circumference * (1 - progress)}`;
    const count = $("dl-count");
    count.hidden = active < 2;
    count.textContent = active > 1 ? String(active) : "";
    button.title = active === 0
      ? "Downloads"
      : active === 1 ? "Downloads (1 downloading)" : `Downloads (${active} downloading)`;
  }

  function render() {
    renderButton();
    if (panel.hidden) return;
    const data = downloads();
    $("dl-list").replaceChildren(...data.items.map(renderItem));
    $("dl-empty").hidden = data.items.length > 0;
    $("dl-ask").setAttribute("aria-pressed", String(!!data.askWhereToSave));
    $("dl-folder").textContent = data.askWhereToSave
      ? "Stillwater asks for a name and folder when a download starts."
      : `Files are saved to ${data.folder || "your Downloads folder"}.`;
    $("dl-clear").disabled = !data.items.some((item) => item.state !== "downloading");
  }

  function openPanel() {
    confirmOpen = null;
    panel.hidden = false;
    send("dl-refresh");
    render();
  }

  function closePanel() {
    panel.hidden = true;
    confirmOpen = null;
  }

  // Render after the rest of the sidebar whenever the state changes.
  const setState = window.sidebar.setState;
  window.sidebar.setState = (newState) => {
    setState(newState);
    render();
  };
  window.sidebar.openDownloads = openPanel;

  button.addEventListener("click", () => (panel.hidden ? openPanel() : closePanel()));
  $("downloads-close").addEventListener("click", closePanel);
  $("dl-open-folder").addEventListener("click", () => send("dl-open-folder"));
  $("dl-ask").addEventListener("click", () => send("dl-ask", downloads().askWhereToSave ? "0" : "1"));
  $("dl-clear").addEventListener("click", () => send("dl-clear"));
  document.addEventListener("keydown", (event) => {
    if (event.key === "Escape" && !panel.hidden) closePanel();
  });
  render();
})();
