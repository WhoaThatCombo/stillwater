<div align="center">

<img src=".github/assets/stillwater-icon.png" width="96" alt="Stillwater icon: a calm wave in a dark circle">

# Stillwater

**A calm, lightweight browser built on [Servo](https://servo.org) — vertical tabs and workspaces, written in Rust.**

[Download for Windows](https://github.com/WhoaThatCombo/stillwater/releases) · [Website](https://whoathatcombo.github.io/stillwater/) · [Build it](#build-on-windows) · [How it works](#how-it-works) · [Credits](#credits)

![Stillwater with a blue-tinted "Work" space](.github/assets/stillwater-workspaces.png)

</div>

> [!WARNING]
> Stillwater is an experimental hobby project. Servo is still maturing, so some sites will render
> incorrectly or break. There are no extensions, no DRM video, and no saved sessions yet.

## What it is

Stillwater is a fork of Servo's demo browser, *servoshell*. Its toolbar is replaced with a vertical sidebar
in the style of [Zen Browser](https://github.com/zen-browser/desktop). Servo, the engine underneath, is
written entirely in Rust, so there's no Chromium or Gecko inside.

The sidebar itself is an HTML/CSS page that **Servo renders for its own UI**, the same way Firefox
renders its interface with Gecko.

## Download

Get the latest Windows build (x64) from [Releases](https://github.com/WhoaThatCombo/stillwater/releases):

- **`Stillwater-Setup-<version>.exe`** (recommended): installs just for you, with no admin prompt.
  It adds Start menu and desktop shortcuts and an uninstaller, registers Stillwater as a browser, and
  keeps Stillwater up to date.
- **`Stillwater-<version>-windows-x64.zip`**: a portable copy. Extract it and run `Stillwater\Stillwater.exe`.

To make Stillwater your default browser, open **Settings → Apps → Default apps**, pick **Stillwater**, and
set it for HTTP, HTTPS and .html. Links you open from other apps arrive as new tabs in the Stillwater
window you already have open.

Installed copies check GitHub for new versions on launch. When one is available, an **Update** button
appears at the bottom of the sidebar; one click downloads it, checks its SHA-256, and restarts Stillwater
on the new version. The build isn't code-signed, so Windows SmartScreen may ask you to confirm the
first install.

## Features

- **Vertical tabs**: favicons, a close button on hover, middle-click to close, double-click a title to rename.
- **Essentials**: favicon tiles at the top of the sidebar, shared by every space.
- **Pinned tabs and folders**: each space has a pinned section above its tabs, with collapsible folders.
  Pinned tabs are remembered and load only when you click them; closing one unloads it to free memory,
  and a reset button takes it back to its pinned page.
- **Tab menu**: right-click a tab (or use its `…` button) to pin it, add it to Essentials, move it to a folder or
  another space, rename it or close it.
- **Session restore**: your tabs in every space come back when you start Stillwater again, even after a crash.
  Only the tab you were on loads; the others load when you click them. Settings → **On startup** can open a
  new tab instead. <kbd>Ctrl</kbd>+<kbd>Shift</kbd>+<kbd>T</kbd> (or **Reopen closed tab** in the tab menu) brings
  back the last closed tab, up to 25.
- **Drag and drop**: drag tabs to reorder them, onto the pinned section or a folder to pin them, onto Essentials,
  back into the tab list to unpin them, or onto a space's button to move them there.
- **URL bar in the sidebar**: type an address or a search and press Enter. <kbd>Ctrl</kbd>+<kbd>L</kbd> focuses it.
  As you type it suggests open tabs (in any space), pinned tabs, bookmarks and your history; arrow keys and Enter pick one.
- **New tab like Zen**: <kbd>Ctrl</kbd>+<kbd>T</kbd> or **New Tab** opens a floating address bar over the page; a tab is
  only created when you pick something (Escape or a click elsewhere cancels). Settings → **New tab opens** switches
  back to a New Tab page.
- **History**: pages you visit are kept on this computer for 90 days. <kbd>Ctrl</kbd>+<kbd>H</kbd> shows them by day, with
  search, deleting single pages and **Clear browsing history** (last hour, today or everything, optionally with cookies
  and site data).
- **Bookmarks**: the star in the URL bar or <kbd>Ctrl</kbd>+<kbd>D</kbd> bookmarks the page (click again to rename or remove);
  the History panel has a searchable Bookmarks list.
- **Workspaces (spaces)**: each tab belongs to one space. Each space has its own name, icon and accent
  colour, and the colour tints the whole window. Space definitions are saved between launches.
- **Compact mode**: the sidebar hides and slides back in when your mouse touches the left edge.
- **Rounded content card**: web pages sit in an inset, rounded frame, like Zen.
- **No title bar**: like Zen, the page runs to the top edge. Touch the top of the window and a slim bar with
  minimise, maximise and close slides in; drag it (or empty sidebar space) to move the window
  (`--native-titlebar` brings the Windows title bar back).
- **Site compatibility**: Servo's experimental web features are on, small built-in shims fill web APIs Servo
  lacks (`ports/servoshell/desktop/compat.js`), and a disk cache speeds up revisits. **Report site isn't working**
  in a tab's menu opens a pre-filled GitHub issue; `tools/sitecheck` sweeps sites and diagnoses failures.
- **Saved passwords**: after you sign in, Stillwater offers to save the password and fills it in next time
  (the key in the URL bar picks between several logins). Settings → **Saved passwords** lists, shows,
  copies and deletes them. They're encrypted with your Windows account (DPAPI), and copied passwords are
  kept out of Windows' clipboard history.
- **Import from Zen**: Settings → **Import from Zen** copies Zen Browser's spaces, pinned tabs, Essentials,
  folders, open tabs, saved passwords, bookmarks and history (last 90 days) from this computer. Zen's profile is only read. Passwords protected
  by a Zen primary password can't be imported.
- **Settings** (gear at the bottom of the sidebar, or <kbd>Ctrl</kbd>+<kbd>,</kbd>): light, dark or system
  appearance (websites that support dark mode follow along), your search engine (DuckDuckGo, Google,
  Bing or Brave Search), compact mode, and checking for updates.

![Stillwater's default sidebar](.github/assets/stillwater-sidebar.png)

## Keyboard shortcuts

| Shortcut | Action |
|---|---|
| <kbd>Ctrl</kbd>+<kbd>L</kbd> / <kbd>Alt</kbd>+<kbd>D</kbd> | Focus the URL bar |
| <kbd>Ctrl</kbd>+<kbd>T</kbd> | New tab in the current space (floating address bar; again to close it) |
| <kbd>Ctrl</kbd>+<kbd>H</kbd> | History |
| <kbd>Ctrl</kbd>+<kbd>D</kbd> | Bookmark this page |
| <kbd>Ctrl</kbd>+<kbd>W</kbd> | Close tab (stays in the current space) |
| <kbd>Ctrl</kbd>+<kbd>Shift</kbd>+<kbd>T</kbd> | Reopen the last closed tab |
| <kbd>Ctrl</kbd>+<kbd>1</kbd>–<kbd>8</kbd>, <kbd>Ctrl</kbd>+<kbd>9</kbd> | Go to tab *n* / the last tab in this space |
| <kbd>Ctrl</kbd>+<kbd>PgUp</kbd> / <kbd>PgDn</kbd> | Previous / next tab in this space |
| <kbd>Ctrl</kbd>+<kbd>Alt</kbd>+<kbd>1</kbd>–<kbd>9</kbd> | Switch to space *n* |
| <kbd>Ctrl</kbd>+<kbd>Alt</kbd>+<kbd>C</kbd> | Toggle compact mode |
| <kbd>Ctrl</kbd>+<kbd>,</kbd> | Open settings |
| <kbd>Alt</kbd>+<kbd>←</kbd> / <kbd>→</kbd> | Back / forward |
| <kbd>Ctrl</kbd>+<kbd>R</kbd> / <kbd>F5</kbd> | Reload |

In the sidebar's space header, click the **icon** to change it, the **name** to rename the space, and the
**dot** to change its colour. The trash icon (click twice) deletes a space and closes its tabs.

## Build on Windows

Stillwater builds the same way as Servo. The full guide is in the
[Servo book](https://book.servo.org/building/building.html).

**You need:** Git, [Rust via rustup](https://rustup.rs), [uv](https://docs.astral.sh/uv/), and Visual
Studio (2022 or newer) with the *Desktop development with C++* workload.

```powershell
git clone https://github.com/WhoaThatCombo/stillwater.git
cd stillwater
.\mach bootstrap          # installs LLVM, CMake, ninja, GStreamer, ...
.\mach build --release    # first build is long; rebuilds are quick
.\target\release\servoshell.exe https://servo.org   # local builds keep Servo's binary name
```

**Common problems:**
- **Wrong Rust toolchain:** Servo needs the **MSVC** toolchain on Windows. If your default is `*-windows-gnu`,
  run `rustup set default-host x86_64-pc-windows-msvc`, or add a directory override in the repo.
- **MinGW on your PATH:** if MinGW's `gcc` is on your `PATH`, remove it for the build so the MSVC
  compiler is used.

## How it works

```
┌────────────── winit window (egui composites the pieces) ──────────────┐
│ ┌── sidebar WebView ──┐  ┌──────── active tab WebView ───────────────┐ │
│ │ servo:sidebar/      │  │                                           │ │
│ │  index.html         │  │       the web page, as a rounded card     │ │
│ │  zen-sidebar.css    │  │                                           │ │
│ │  sidebar.js         │  │                                           │ │
│ └─────────────────────┘  └───────────────────────────────────────────┘ │
└────────────────────────────────────────────────────────────────────────┘
```

- **Sidebar rendering:** the sidebar is a second Servo `WebView` with its own offscreen rendering
  context. egui only places the sidebar and the page in the window and draws native dialogs.
- **Page → Rust:** the sidebar page sends `console.log("__sidebar__<command>\t<arg>")`.
- **Rust → page:** Rust calls `webview.evaluate_javascript("window.sidebar.setState({...})")` with
  the tab list, navigation state and spaces.
- **Input:** mouse input over the sidebar goes to the sidebar. Keyboard input goes to whichever side
  was clicked last.

| File | Role |
|---|---|
| [`ports/servoshell/desktop/sidebar.rs`](ports/servoshell/desktop/sidebar.rs) | Sidebar WebView, messaging, layout, compact mode |
| [`ports/servoshell/desktop/workspaces.rs`](ports/servoshell/desktop/workspaces.rs) | Spaces: tab ownership, switching, colours, persistence |
| [`ports/servoshell/desktop/passwords.rs`](ports/servoshell/desktop/passwords.rs) | Saved passwords: saving, filling, encrypted storage |
| [`ports/servoshell/desktop/zen_import.rs`](ports/servoshell/desktop/zen_import.rs) | Reading Zen's spaces, tabs, passwords, bookmarks and history |
| [`ports/servoshell/desktop/history.rs`](ports/servoshell/desktop/history.rs) | History (SQLite) and address-bar suggestions |
| [`ports/servoshell/desktop/bookmarks.rs`](ports/servoshell/desktop/bookmarks.rs) | Bookmarks (JSON) |
| [`ports/servoshell/desktop/command_bar.rs`](ports/servoshell/desktop/command_bar.rs) | Zen's floating new-tab address bar |
| [`ports/servoshell/desktop/gui.rs`](ports/servoshell/desktop/gui.rs) | Compositing the sidebar and content |
| [`ports/servoshell/desktop/headed_window.rs`](ports/servoshell/desktop/headed_window.rs) | Input routing and shortcuts |
| [`resources/resource_protocol/sidebar/`](resources/resource_protocol/sidebar/) | The sidebar's HTML, CSS and JS |

## Roadmap

- [x] Split view (two pages side by side)
- [x] Session restore
- [x] Drag tabs between spaces, pinned tabs, folders
- [ ] Command palette
- [x] Prebuilt Windows downloads
- [x] Installer, default-browser registration and auto-updates
- [x] Built-in adblocker using uBlock Origin's filter lists
- [x] Essentials, pinned tabs, folders
- [x] Split view and Glance
- [x] Bookmarks, history and address-bar suggestions
- [ ] Downloads, find in page
- [x] Import from Zen
- [x] Saved passwords
- [x] Stillwater name and icon in the window and installer

## Credits

- **[Servo](https://github.com/servo/servo)**: the engine and the servoshell base, © The Servo Project
  Developers, [MPL-2.0](LICENSE). Servo's original README is in [`SERVO_README.md`](SERVO_README.md).
- **[Zen Browser](https://github.com/zen-browser/desktop)**: Stillwater's colours, radii and tab
  styling are adapted from Zen's MPL-2.0 stylesheets (credited in
  [`zen-sidebar.css`](resources/resource_protocol/sidebar/zen-sidebar.css)).

Stillwater is not affiliated with or endorsed by the Servo project or Zen Browser.

## License

[Mozilla Public License 2.0](LICENSE), the same as Servo and Zen.
