/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Zen-style workspaces ("spaces"): every tab belongs to exactly one workspace, the sidebar
//! only lists the current workspace's tabs, and each workspace has its own icon and accent
//! colour. Workspace definitions (not their tabs) persist in `<config dir>/workspaces.json`.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use log::warn;
use serde_json::{Value, json};
use servo::{Theme, WebViewId};

use crate::running_app_state::{PreloadTarget, UserInterfaceCommand};
use crate::window::ServoShellWindow;

/// Session restore, reopening closed tabs, drag and drop.
mod session;
/// Loading the last session's tabs in the background at startup.
mod preload;

/// Segoe Fluent Icons glyphs offered as workspace icons.
const ICONS: &[&str] = &[
    "\u{E80F}", // Home
    "\u{E821}", // Work
    "\u{E7FC}", // Game
    "\u{E8D6}", // Audio
    "\u{E734}", // Star
    "\u{EB51}", // Heart
    "\u{E774}", // Globe
    "\u{EA80}", // Lightbulb
    "\u{E719}", // Shop
    "\u{E943}", // Code
    "\u{E709}", // Airplane
    "\u{E722}", // Camera
];

/// Accent colours; index 0 means "no tint".
const COLORS: &[Option<[u8; 3]>] = &[
    None,
    Some([100, 115, 255]), // blue
    Some([160, 100, 255]), // purple
    Some([255, 90, 160]),  // pink
    Some([255, 140, 60]),  // orange
    Some([60, 190, 120]),  // green
    Some([40, 180, 200]),  // teal
    Some([240, 80, 80]),   // red
];

struct Workspace {
    id: u32,
    name: String,
    icon: usize,
    color: usize,
}

impl Workspace {
    fn from_json(value: &Value) -> Option<Self> {
        Some(Self {
            id: value.get("id")?.as_u64()? as u32,
            name: value.get("name")?.as_str()?.to_owned(),
            icon: value.get("icon")?.as_u64()? as usize % ICONS.len(),
            color: value.get("color")?.as_u64()? as usize % COLORS.len(),
        })
    }
}

/// The pseudo-space that Essentials live in: they show in every space.
const ESSENTIALS: u32 = 0;

/// A pinned tab or Essential. It remembers its URL even while unloaded (no `WebView`), so it
/// costs no memory until clicked, like Zen's pinned tabs.
struct Pin {
    id: u32,
    url: String,
    title: String,
    custom_title: Option<String>,
    favicon: Option<String>,
    /// `ESSENTIALS` or the id of the space it's pinned in.
    space: u32,
    folder: Option<u32>,
    webview: Option<WebViewId>,
}

impl Pin {
    fn from_json(value: &Value) -> Option<Self> {
        Some(Self {
            id: value.get("id")?.as_u64()? as u32,
            url: value.get("url")?.as_str()?.to_owned(),
            title: value["title"].as_str().unwrap_or_default().to_owned(),
            custom_title: value["customTitle"].as_str().map(str::to_owned),
            favicon: value["favicon"].as_str().map(str::to_owned),
            space: value.get("space")?.as_u64()? as u32,
            folder: value["folder"].as_u64().map(|id| id as u32),
            webview: None,
        })
    }

    fn to_json(&self) -> Value {
        json!({
            "id": self.id, "url": self.url, "title": self.title,
            "customTitle": self.custom_title, "favicon": self.favicon,
            "space": self.space, "folder": self.folder,
        })
    }
}

/// An ordinary tab whose page was unloaded to free memory (Zen's tab unloader). It keeps its
/// place, title and icon in the sidebar and loads its page again when activated.
struct UnloadedTab {
    /// The sidebar's id for the tab, kept so the row stays the same while it's unloaded.
    key: u32,
    url: String,
    title: String,
    custom_title: Option<String>,
    favicon: Option<String>,
    space: u32,
    order: u64,
    /// When it was last the active tab (milliseconds since 1970), if known: background loading
    /// at startup takes the most recently used first.
    used: Option<u64>,
}

/// A tab in the sidebar's tab list: open, or unloaded.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum TabEntry {
    Live(WebViewId),
    /// The sidebar key of an unloaded tab.
    Unloaded(u32),
}

/// A folder of pinned tabs inside a space.
struct Folder {
    id: u32,
    space: u32,
    name: String,
    collapsed: bool,
}

impl Folder {
    fn from_json(value: &Value) -> Option<Self> {
        Some(Self {
            id: value.get("id")?.as_u64()? as u32,
            space: value.get("space")?.as_u64()? as u32,
            name: value.get("name")?.as_str()?.to_owned(),
            collapsed: value["collapsed"].as_bool().unwrap_or(false),
        })
    }
}

pub(crate) struct Workspaces {
    list: Vec<Workspace>,
    current: u32,
    next_id: u32,
    pins: Vec<Pin>,
    folders: Vec<Folder>,
    next_pin_id: u32,
    /// A pinned tab is being opened; the next new `WebView` belongs to it.
    pending_pin: Option<u32>,
    /// Session-only names for regular tabs (double-click to rename).
    custom_titles: HashMap<WebViewId, String>,
    /// The page a reloaded tab was asked to load, until it has a URL of its own (shown and
    /// copied while it loads).
    requested_urls: HashMap<WebViewId, String>,
    /// Which workspace each open tab belongs to.
    tab_workspace: HashMap<WebViewId, u32>,
    /// The tab to show again when switching back to a workspace.
    last_active: HashMap<u32, WebViewId>,
    /// A new tab was requested for the current workspace; until it appears, don't follow the
    /// (old) active tab into another workspace.
    awaiting_new_tab: bool,
    path: Option<PathBuf>,
    /// Position of each open tab in its list (tabs loaded again after unloading keep their place).
    tab_order: HashMap<WebViewId, u64>,
    next_order: u64,
    /// When each open tab was last the active one (or was opened).
    last_used: HashMap<WebViewId, Instant>,
    /// Ordinary tabs whose pages were unloaded.
    unloaded: Vec<UnloadedTab>,
    /// An unloaded tab is being loaded again; the next new `WebView` belongs to it.
    pending_restore: Option<u32>,
    /// Tabs that were loaded again since the sidebar last asked: (new WebView, sidebar key,
    /// favicon), so the sidebar keeps showing the same row.
    restored: Vec<(WebViewId, u32, Option<String>)>,
    /// Recently closed ordinary tabs, newest last (Ctrl+Shift+T).
    closed: Vec<UnloadedTab>,
    /// For each space not visited since launch: the restored tab that was active in it.
    session_active: HashMap<u32, u32>,
    /// Spaces are turned on (Settings → Tabs). Off, there's one list of tabs, untinted.
    enabled: bool,
    /// Loading the restored tabs in the background, while that's going on.
    preload: Option<preload::Preloader>,
}

impl Workspaces {
    pub(crate) fn load() -> Self {
        let path = crate::desktop::settings::config_dir().map(|dir| dir.join("workspaces.json"));
        let saved = path
            .as_ref()
            .and_then(|path| std::fs::read_to_string(path).ok())
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .unwrap_or(Value::Null);
        // 0.2.x saved a bare array of spaces; newer versions save {spaces, pins, folders}.
        let spaces = if saved.is_array() { &saved } else { &saved["spaces"] };
        let parse_list = |value: &Value| -> Vec<Value> { value.as_array().cloned().unwrap_or_default() };
        let pins: Vec<Pin> = parse_list(&saved["pins"]).iter().filter_map(Pin::from_json).collect();
        let folders: Vec<Folder> = parse_list(&saved["folders"])
            .iter()
            .filter_map(Folder::from_json)
            .collect();
        let list: Vec<Workspace> = Some(parse_list(spaces))
            .map(|items| items.iter().filter_map(Workspace::from_json).collect::<Vec<_>>())
            .filter(|list: &Vec<Workspace>| !list.is_empty())
            .unwrap_or_else(|| {
                vec![Workspace {
                    id: 1,
                    name: "Home".into(),
                    icon: 0,
                    color: 0,
                }]
            });
        let next_id = list.iter().map(|workspace| workspace.id).max().unwrap_or(0) + 1;
        let next_pin_id = pins
            .iter()
            .map(|pin| pin.id)
            .chain(folders.iter().map(|folder| folder.id))
            .max()
            .unwrap_or(0) +
            1;
        Self {
            current: list[0].id,
            list,
            next_id,
            pins,
            folders,
            next_pin_id,
            pending_pin: None,
            custom_titles: Default::default(),
            requested_urls: Default::default(),
            tab_workspace: Default::default(),
            last_active: Default::default(),
            awaiting_new_tab: false,
            path,
            tab_order: Default::default(),
            next_order: 0,
            last_used: Default::default(),
            unloaded: Vec::new(),
            pending_restore: None,
            restored: Vec::new(),
            closed: Vec::new(),
            session_active: Default::default(),
            enabled: true,
            preload: None,
        }
        .with_spaces(crate::desktop::settings::Settings::load().spaces)
    }

    fn with_spaces(mut self, enabled: bool) -> Self {
        self.set_enabled(enabled);
        self
    }

    /// Turn spaces on or off. Nothing is merged or deleted: with spaces off, every space's
    /// tabs, pinned tabs and folders show in one list (new ones go to the first space), and
    /// turning spaces back on shows each in its own space again.
    pub(crate) fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        if !enabled {
            self.current = self.list[0].id;
        }
    }

    /// Whether the sidebar lists `space`'s tabs: the current space's, or every space's while
    /// spaces are off. Essentials are never in the tab list.
    fn in_view(&self, space: u32) -> bool {
        if self.enabled {
            space == self.current
        } else {
            space != ESSENTIALS
        }
    }

    /// The tabs the sidebar lists (open and unloaded), in sidebar order.
    fn view_entries(&self, window: &ServoShellWindow) -> Vec<TabEntry> {
        if self.enabled {
            return self.entries_in(window, self.current);
        }
        let mut entries: Vec<(u64, TabEntry)> = window
            .webview_ids()
            .into_iter()
            .filter(|id| self.tab_workspace.get(id).is_some_and(|space| self.in_view(*space)))
            .map(|id| (self.order_of(id), TabEntry::Live(id)))
            .collect();
        entries.extend(
            self.unloaded
                .iter()
                .filter(|tab| self.in_view(tab.space))
                .map(|tab| (tab.order, TabEntry::Unloaded(tab.key))),
        );
        entries.sort_by_key(|(order, _)| *order);
        entries.into_iter().map(|(_, entry)| entry).collect()
    }

    fn save(&self) {
        let Some(path) = &self.path else {
            return;
        };
        let list: Vec<Value> = self
            .list
            .iter()
            .map(|workspace| {
                json!({
                    "id": workspace.id,
                    "name": workspace.name,
                    "icon": workspace.icon,
                    "color": workspace.color,
                })
            })
            .collect();
        let folders: Vec<Value> = self
            .folders
            .iter()
            .map(|folder| {
                json!({ "id": folder.id, "space": folder.space, "name": folder.name, "collapsed": folder.collapsed })
            })
            .collect();
        let data = json!({
            "spaces": list,
            "pins": self.pins.iter().map(Pin::to_json).collect::<Vec<_>>(),
            "folders": folders,
        });
        let result = path
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|_| std::fs::write(path, data.to_string()));
        if let Err(error) = result {
            warn!("Could not save workspaces to {}: {error}", path.display());
        }
    }

    fn get_mut(&mut self, id: u32) -> Option<&mut Workspace> {
        self.list.iter_mut().find(|workspace| workspace.id == id)
    }

    fn current_workspace(&self) -> Option<&Workspace> {
        self.list.iter().find(|workspace| workspace.id == self.current)
    }

    /// Open tabs belonging to `workspace`, in sidebar order.
    fn tabs_in(&self, window: &ServoShellWindow, workspace: u32) -> Vec<WebViewId> {
        let mut tabs: Vec<WebViewId> = window
            .webview_ids()
            .into_iter()
            .filter(|id| self.tab_workspace.get(id) == Some(&workspace))
            .collect();
        tabs.sort_by_key(|id| self.order_of(*id));
        tabs
    }

    /// When `webview` was last used, in milliseconds since 1970 (saved with the session).
    fn used_millis(&self, webview: WebViewId) -> Option<u64> {
        let used = self.last_used.get(&webview)?;
        let when = std::time::SystemTime::now().checked_sub(used.elapsed())?;
        when.duration_since(std::time::UNIX_EPOCH)
            .ok()
            .map(|since| since.as_millis() as u64)
    }

    fn order_of(&self, webview: WebViewId) -> u64 {
        self.tab_order.get(&webview).copied().unwrap_or(u64::MAX)
    }

    /// Open and unloaded tabs of `workspace`, in sidebar order. Open pinned tabs are included
    /// (as before); unloaded pins live in `pins`, not here.
    fn entries_in(&self, window: &ServoShellWindow, workspace: u32) -> Vec<TabEntry> {
        let mut entries: Vec<(u64, TabEntry)> = self
            .tabs_in(window, workspace)
            .into_iter()
            .map(|id| (self.order_of(id), TabEntry::Live(id)))
            .collect();
        entries.extend(
            self.unloaded
                .iter()
                .filter(|tab| tab.space == workspace)
                .map(|tab| (tab.order, TabEntry::Unloaded(tab.key))),
        );
        entries.sort_by_key(|(order, _)| *order);
        entries.into_iter().map(|(_, entry)| entry).collect()
    }

    /// Show a tab, loading it again first if it was unloaded.
    pub(crate) fn activate_entry(&mut self, window: &ServoShellWindow, entry: TabEntry) {
        match entry {
            TabEntry::Live(id) => window.activate_webview(id),
            TabEntry::Unloaded(key) => self.restore_tab(window, key),
        }
    }

    /// Assign new tabs to the current workspace, forget closed ones, and follow the active tab
    /// if something (e.g. a link opening a popup) activated a tab in another workspace.
    pub(crate) fn sync(&mut self, window: &ServoShellWindow) {
        let ids = window.webview_ids();
        self.tab_workspace.retain(|id, _| ids.contains(id));
        self.tab_order.retain(|id, _| ids.contains(id));
        self.last_used.retain(|id, _| ids.contains(id));
        self.last_active.retain(|_, id| ids.contains(id));
        self.custom_titles.retain(|id, _| ids.contains(id));
        self.requested_urls.retain(|id, _| {
            ids.contains(id) && window.webview_by_id(*id).is_some_and(|webview| webview.url().is_none())
        });
        // A closed pinned tab is unloaded, not removed.
        for pin in &mut self.pins {
            if pin.webview.is_some_and(|id| !ids.contains(&id)) {
                pin.webview = None;
            }
        }
        let now = Instant::now();
        let new_preloads: HashMap<WebViewId, PreloadTarget> =
            window.take_new_preloads().into_iter().collect();
        for id in &ids {
            if !self.tab_workspace.contains_key(id) {
                // A restored tab loaded in the background at startup.
                if let Some(target) = new_preloads.get(id) {
                    self.adopt_preload(window, *id, *target);
                    if !self.tab_order.contains_key(id) {
                        self.next_order += 1;
                        self.tab_order.insert(*id, self.next_order);
                    }
                    self.last_used.insert(*id, now);
                    continue;
                }
                // An unloaded tab loading again, the tab a pinned tab asked for, or an
                // ordinary new tab.
                let restored = self.pending_restore.take().and_then(|key| {
                    let position = self.unloaded.iter().position(|tab| tab.key == key)?;
                    Some(self.unloaded.remove(position))
                });
                // A tab the browser loads by itself (restored from the session, or unloaded and
                // loaded again because it was selected, or a pinned tab's first load) must not
                // start playing media until the user interacts with it.
                let quiet = restored.is_some() || self.pending_pin.is_some();
                if quiet && let Some(webview) = window.webview_by_id(*id) {
                    webview.block_autoplay_until_activation();
                }
                if let Some(tab) = &restored {
                    self.requested_urls.insert(*id, tab.url.clone());
                }
                let space = if let Some(tab) = restored {
                    self.tab_order.insert(*id, tab.order);
                    if let Some(title) = tab.custom_title {
                        self.custom_titles.insert(*id, title);
                    }
                    self.restored.push((*id, tab.key, tab.favicon));
                    tab.space
                } else {
                    let pin = self
                        .pending_pin
                        .take()
                        .and_then(|pin_id| self.pins.iter_mut().find(|pin| pin.id == pin_id));
                    match pin {
                        Some(pin) => {
                            pin.webview = Some(*id);
                            pin.space
                        },
                        None => self.current,
                    }
                };
                self.tab_workspace.insert(*id, space);
                if !self.tab_order.contains_key(id) {
                    self.next_order += 1;
                    self.tab_order.insert(*id, self.next_order);
                }
                self.last_used.insert(*id, now);
                self.awaiting_new_tab = false;
            }
        }
        if let Some(active) = window.active_webview().map(|webview| webview.id()) {
            self.last_used.insert(active, now);
            let workspace = self.tab_workspace[&active];
            if !self.enabled {
                // One list: stay in the first space, but remember each space's active tab
                // for when spaces are turned back on.
                if workspace != ESSENTIALS {
                    self.last_active.insert(workspace, active);
                }
                return;
            }
            // Essentials belong to every space, so they never switch spaces.
            if workspace != self.current && workspace != ESSENTIALS && !self.awaiting_new_tab {
                self.current = workspace;
            }
            if workspace == self.current || workspace == ESSENTIALS {
                self.last_active.insert(self.current, active);
            }
        }
    }

    pub(crate) fn open_tab(&mut self, window: &ServoShellWindow) {
        window.queue_user_interface_command(UserInterfaceCommand::NewWebView);
        self.awaiting_new_tab = true;
    }

    /// Close a tab, keeping the user in the same workspace: activate a sibling first, or open a
    /// fresh tab if it was the workspace's last one.
    pub(crate) fn close_tab(&mut self, window: &ServoShellWindow, id: WebViewId) {
        if window.active_webview().map(|webview| webview.id()) == Some(id) {
            let workspace = self.tab_workspace.get(&id).copied().unwrap_or(self.current);
            let workspace = if workspace == ESSENTIALS { self.current } else { workspace };
            let entries = if self.enabled {
                self.entries_in(window, workspace)
            } else {
                self.view_entries(window)
            };
            // Like Firefox and Chrome: the tab below the closed one, or the one above if it
            // was the last (not the bottom of the list, which may be far away and unloaded).
            let position = entries.iter().position(|other| *other == TabEntry::Live(id));
            let neighbour = match position {
                Some(position) => entries
                    .get(position + 1)
                    .or_else(|| position.checked_sub(1).and_then(|above| entries.get(above))),
                None => entries.iter().rev().find(|other| **other != TabEntry::Live(id)),
            };
            match neighbour.copied() {
                Some(sibling) => self.activate_entry(window, sibling),
                None => self.open_tab(window),
            }
        }
        window.queue_user_interface_command(UserInterfaceCommand::CloseWebView(id));
    }

    /// Activate the `index`-th tab of the current workspace (`usize::MAX` = last).
    pub(crate) fn activate_tab_index(&mut self, window: &ServoShellWindow, index: usize) {
        let tabs = self.view_entries(window);
        let tab = if index == usize::MAX { tabs.last() } else { tabs.get(index) };
        if let Some(tab) = tab.copied() {
            self.activate_entry(window, tab);
        }
    }

    /// Activate the next (`delta` = 1) or previous (`delta` = -1) tab in the current workspace.
    pub(crate) fn cycle_tab(&mut self, window: &ServoShellWindow, delta: isize) {
        let tabs = self.view_entries(window);
        let Some(active) = window.active_webview().map(|webview| webview.id()) else {
            return;
        };
        let next = match tabs.iter().position(|entry| *entry == TabEntry::Live(active)) {
            Some(position) => Some((position as isize + delta).rem_euclid(tabs.len() as isize) as usize),
            // From a pinned tab or an Essential (not in this list): the first or last tab.
            None if tabs.is_empty() => None,
            None if delta > 0 => Some(0),
            None => Some(tabs.len() - 1),
        };
        if let Some(next) = next {
            self.activate_entry(window, tabs[next]);
        }
    }

    pub(crate) fn switch_to(&mut self, window: &ServoShellWindow, workspace: u32) {
        if !self.enabled || workspace == self.current || self.get_mut(workspace).is_none() {
            return;
        }
        self.current = workspace;
        let tabs = self.tabs_in(window, workspace);
        let target = self
            .last_active
            .get(&workspace)
            .copied()
            .filter(|id| tabs.contains(id))
            .or(tabs.last().copied());
        match target {
            Some(id) => window.activate_webview(id),
            // Only unloaded tabs left: load the one that was active last session, or the
            // last one, rather than opening a new tab.
            None => match self
                .take_session_active(workspace)
                .or_else(|| self.entries_in(window, workspace).last().copied())
            {
                Some(entry) => self.activate_entry(window, entry),
                None => self.open_tab(window),
            },
        }
    }

    pub(crate) fn switch_to_index(&mut self, window: &ServoShellWindow, index: usize) {
        if let Some(workspace) = self.list.get(index).map(|workspace| workspace.id) {
            self.switch_to(window, workspace);
        }
    }

    pub(crate) fn create(&mut self, window: &ServoShellWindow) {
        let count = self.list.len();
        let id = self.next_id;
        self.next_id += 1;
        self.list.push(Workspace {
            id,
            name: format!("Space {}", count + 1),
            icon: count % ICONS.len(),
            color: count % (COLORS.len() - 1) + 1,
        });
        self.save();
        self.switch_to(window, id);
    }

    pub(crate) fn rename(&mut self, id: u32, name: &str) {
        let name: String = name.trim().chars().take(32).collect();
        if let Some(workspace) = self.get_mut(id) &&
            !name.is_empty()
        {
            workspace.name = name;
            self.save();
        }
    }

    pub(crate) fn cycle_icon(&mut self, id: u32) {
        if let Some(workspace) = self.get_mut(id) {
            workspace.icon = (workspace.icon + 1) % ICONS.len();
            self.save();
        }
    }

    pub(crate) fn cycle_color(&mut self, id: u32) {
        if let Some(workspace) = self.get_mut(id) {
            workspace.color = (workspace.color + 1) % COLORS.len();
            self.save();
        }
    }

    /// Delete a workspace and close its tabs. The last workspace can't be deleted.
    pub(crate) fn delete(&mut self, window: &ServoShellWindow, id: u32) {
        let Some(position) = self.list.iter().position(|workspace| workspace.id == id) else {
            return;
        };
        if self.list.len() <= 1 {
            return;
        }
        let doomed = self.tabs_in(window, id);
        if self.current == id {
            let neighbour = self.list[if position == 0 { 1 } else { position - 1 }].id;
            self.switch_to(window, neighbour);
        }
        self.list.remove(position);
        self.last_active.remove(&id);
        // The space's pinned tabs, folders and unloaded tabs go with it.
        self.pins.retain(|pin| pin.space != id);
        self.unloaded.retain(|tab| tab.space != id);
        self.folders.retain(|folder| folder.space != id);
        self.save();
        for tab in doomed {
            window.queue_user_interface_command(UserInterfaceCommand::CloseWebView(tab));
        }
    }

    /// The window background: Zen's `--zen-main-browser-background`, tinted with the current
    /// workspace's accent colour.
    pub(crate) fn background(&self, theme: Theme) -> [u8; 3] {
        let (base, amount) = match theme {
            Theme::Dark => ([27u8, 27, 27], 0.16),
            Theme::Light => ([235u8, 235, 235], 0.22),
        };
        let Some(accent) = self
            .current_workspace()
            .filter(|_| self.enabled)
            .and_then(|workspace| COLORS[workspace.color])
        else {
            return base;
        };
        std::array::from_fn(|i| {
            (base[i] as f32 * (1.0 - amount) + accent[i] as f32 * amount).round() as u8
        })
    }

    pub(crate) fn to_json(&self) -> Value {
        let list: Vec<Value> = self
            .list
            .iter()
            .map(|workspace| {
                json!({
                    "id": workspace.id,
                    "name": workspace.name,
                    "icon": ICONS[workspace.icon],
                    "color": COLORS[workspace.color]
                        .filter(|_| self.enabled)
                        .map(|[r, g, b]| format!("rgb({r}, {g}, {b})")),
                })
            })
            .collect();
        json!({ "list": list, "current": self.current, "enabled": self.enabled })
    }
}

/// Pinned tabs, Essentials and folders (Zen's sidebar sections).
impl Workspaces {
    fn pin_mut(&mut self, id: u32) -> Option<&mut Pin> {
        self.pins.iter_mut().find(|pin| pin.id == id)
    }

    fn pin_for_webview(&self, webview: WebViewId) -> Option<&Pin> {
        self.pins.iter().find(|pin| pin.webview == Some(webview))
    }

    fn next_id(&mut self) -> u32 {
        let id = self.next_pin_id;
        self.next_pin_id += 1;
        id
    }

    /// The current space's ordinary (unpinned) tabs, open or unloaded, in sidebar order.
    pub(crate) fn regular_entries(&self, window: &ServoShellWindow) -> Vec<TabEntry> {
        self.view_entries(window)
            .into_iter()
            .filter(|entry| match entry {
                TabEntry::Live(id) => self.pin_for_webview(*id).is_none(),
                TabEntry::Unloaded(_) => true,
            })
            .collect()
    }

    /// The page a tab that is loading again was asked for, while it has no URL yet.
    pub(crate) fn requested_url(&self, webview: WebViewId) -> Option<&String> {
        self.requested_urls.get(&webview)
    }

    /// A regular tab's custom name, if the user renamed it.
    pub(crate) fn custom_title(&self, webview: WebViewId) -> Option<&String> {
        self.custom_titles.get(&webview)
    }

    /// Pin a tab to the current space, or to Essentials (shown in every space).
    pub(crate) fn pin_tab(&mut self, window: &ServoShellWindow, webview: WebViewId, essential: bool) {
        let space = if essential { ESSENTIALS } else { self.current };
        if let Some(pin_id) = self.pin_for_webview(webview).map(|pin| pin.id) {
            if let Some(pin) = self.pin_mut(pin_id) {
                pin.space = space;
                pin.folder = None;
            }
        } else {
            let Some(view) = window.webview_by_id(webview) else {
                return;
            };
            let id = self.next_id();
            self.pins.push(Pin {
                id,
                url: view.url().map(|url| url.to_string()).unwrap_or_default(),
                title: view.page_title().unwrap_or_default(),
                custom_title: self.custom_titles.remove(&webview),
                favicon: None,
                space,
                folder: None,
                webview: Some(webview),
            });
        }
        self.tab_workspace.insert(webview, space);
        self.save();
    }

    /// Turn a pin back into an ordinary tab (or drop it, if it isn't loaded).
    pub(crate) fn unpin(&mut self, pin_id: u32) {
        let Some(position) = self.pins.iter().position(|pin| pin.id == pin_id) else {
            return;
        };
        let pin = self.pins.remove(position);
        if let Some(webview) = pin.webview {
            let space = if pin.space == ESSENTIALS { self.current } else { pin.space };
            self.tab_workspace.insert(webview, space);
            if let Some(title) = pin.custom_title {
                self.custom_titles.insert(webview, title);
            }
        }
        self.save();
    }

    /// Move a pin between Essentials and the current space.
    pub(crate) fn toggle_essential(&mut self, pin_id: u32) {
        let current = self.current;
        let Some(pin) = self.pin_mut(pin_id) else {
            return;
        };
        pin.space = if pin.space == ESSENTIALS { current } else { ESSENTIALS };
        pin.folder = None;
        let (webview, space) = (pin.webview, pin.space);
        if let Some(webview) = webview {
            self.tab_workspace.insert(webview, space);
        }
        self.save();
    }

    /// Show a pinned tab, loading it first if it's unloaded.
    /// The page a pinned tab opens.
    pub(crate) fn pin_url(&self, pin_id: u32) -> Option<url::Url> {
        let pin = self.pins.iter().find(|pin| pin.id == pin_id)?;
        url::Url::parse(&pin.url).ok()
    }

    pub(crate) fn open_pin(&mut self, window: &ServoShellWindow, pin_id: u32) {
        let Some(pin) = self.pins.iter().find(|pin| pin.id == pin_id) else {
            return;
        };
        if let Some(webview) = pin.webview {
            window.activate_webview(webview);
            return;
        }
        let Ok(url) = url::Url::parse(&pin.url) else {
            return;
        };
        if self.activate_when_preloaded(PreloadTarget::Pin(pin_id)) {
            return;
        }
        self.pending_pin = Some(pin_id);
        self.awaiting_new_tab = true;
        window.queue_user_interface_command(UserInterfaceCommand::NewWebViewWithUrl(url));
    }

    /// Close a pinned tab's page to free its memory; the pin stays.
    pub(crate) fn unload_pin(&mut self, window: &ServoShellWindow, pin_id: u32) {
        let webview = self.pins.iter().find(|pin| pin.id == pin_id).and_then(|pin| pin.webview);
        if let Some(webview) = webview {
            self.close_tab(window, webview);
        }
    }

    /// Go back to the page the pin was created with.
    pub(crate) fn reset_pin(&self, window: &ServoShellWindow, pin_id: u32) {
        let Some(pin) = self.pins.iter().find(|pin| pin.id == pin_id) else {
            return;
        };
        if let (Some(webview), Ok(url)) = (
            pin.webview.and_then(|id| window.webview_by_id(id)),
            url::Url::parse(&pin.url),
        ) {
            webview.load(url);
        }
    }

    /// Rename a tab (empty name = back to the page title). Pins remember it across launches.
    pub(crate) fn rename_tab(&mut self, webview: WebViewId, title: &str) {
        let title = title.trim();
        let title = (!title.is_empty()).then(|| title.chars().take(80).collect::<String>());
        if let Some(pin_id) = self.pin_for_webview(webview).map(|pin| pin.id) {
            self.rename_pin(pin_id, title.as_deref().unwrap_or(""));
        } else {
            match title {
                Some(title) => self.custom_titles.insert(webview, title),
                None => self.custom_titles.remove(&webview),
            };
        }
    }

    pub(crate) fn rename_pin(&mut self, pin_id: u32, title: &str) {
        let title = title.trim();
        if let Some(pin) = self.pin_mut(pin_id) {
            pin.custom_title = (!title.is_empty()).then(|| title.chars().take(80).collect());
            self.save();
        }
    }

    /// Create a folder in the current space and return its id.
    pub(crate) fn create_folder(&mut self) -> u32 {
        let id = self.next_id();
        self.folders.push(Folder {
            id,
            space: self.current,
            name: "New folder".into(),
            collapsed: false,
        });
        self.save();
        id
    }

    pub(crate) fn rename_folder(&mut self, folder_id: u32, name: &str) {
        let name: String = name.trim().chars().take(40).collect();
        if let Some(folder) = self.folders.iter_mut().find(|folder| folder.id == folder_id) &&
            !name.is_empty()
        {
            folder.name = name;
            self.save();
        }
    }

    pub(crate) fn toggle_folder(&mut self, folder_id: u32) {
        if let Some(folder) = self.folders.iter_mut().find(|folder| folder.id == folder_id) {
            folder.collapsed = !folder.collapsed;
            self.save();
        }
    }

    /// Delete a folder; its pinned tabs stay pinned, outside any folder.
    pub(crate) fn delete_folder(&mut self, folder_id: u32) {
        self.folders.retain(|folder| folder.id != folder_id);
        for pin in &mut self.pins {
            if pin.folder == Some(folder_id) {
                pin.folder = None;
            }
        }
        self.save();
    }

    /// Put a pinned tab in a folder (`None` = take it out).
    pub(crate) fn move_pin_to_folder(&mut self, pin_id: u32, folder: Option<u32>) {
        let folder = folder.and_then(|id| self.folders.iter().find(|folder| folder.id == id));
        let (folder, folder_space) = (folder.map(|folder| folder.id), folder.map(|folder| folder.space));
        if let Some(pin) = self.pin_mut(pin_id) &&
            pin.space != ESSENTIALS
        {
            pin.folder = folder;
            // With spaces off, folders of every space are listed: the pin joins its folder's.
            if let Some(space) = folder_space {
                pin.space = space;
            }
            let (webview, space) = (pin.webview, pin.space);
            if let Some(webview) = webview {
                self.tab_workspace.insert(webview, space);
            }
            self.save();
        }
    }

    /// Move a pinned tab up (`delta` = -1) or down (1) among its neighbours.
    pub(crate) fn move_pin(&mut self, pin_id: u32, delta: isize) {
        let Some(position) = self.pins.iter().position(|pin| pin.id == pin_id) else {
            return;
        };
        let (space, folder) = (self.pins[position].space, self.pins[position].folder);
        let neighbours: Vec<usize> = (0..self.pins.len())
            .filter(|index| {
                let other = &self.pins[*index];
                // With spaces off, loose pins of every space show as one group.
                let same_group = other.space == space ||
                    (!self.enabled && space != ESSENTIALS && self.in_view(other.space));
                same_group && other.folder == folder
            })
            .collect();
        let Some(index) = neighbours.iter().position(|index| *index == position) else {
            return;
        };
        let target = index as isize + delta;
        if target < 0 || target as usize >= neighbours.len() {
            return;
        }
        self.pins.swap(position, neighbours[target as usize]);
        self.save();
    }

    /// Move a tab (pinned or not) to another space.
    pub(crate) fn move_tab_to_space(&mut self, window: &ServoShellWindow, webview: WebViewId, space: u32) {
        if !self.list.iter().any(|workspace| workspace.id == space) {
            return;
        }
        // Keep the user where they are: show a sibling before the tab leaves.
        if window.active_webview().map(|view| view.id()) == Some(webview) {
            let sibling = self
                .tabs_in(window, self.current)
                .into_iter()
                .rfind(|other| *other != webview);
            match sibling {
                Some(sibling) => window.activate_webview(sibling),
                None => self.open_tab(window),
            }
        }
        if let Some(pin_id) = self.pin_for_webview(webview).map(|pin| pin.id) &&
            let Some(pin) = self.pin_mut(pin_id)
        {
            pin.space = space;
            pin.folder = None;
            self.save();
        }
        self.tab_workspace.insert(webview, space);
    }

    /// Keep pins' saved titles and icons current while their pages are open.
    pub(crate) fn refresh_pins(&mut self, window: &ServoShellWindow, favicons: &HashMap<WebViewId, String>) {
        let mut changed = false;
        for pin in &mut self.pins {
            let Some(webview) = pin.webview else {
                continue;
            };
            if let Some(title) = window.webview_by_id(webview).and_then(|view| view.page_title()) &&
                !title.is_empty() &&
                title != pin.title
            {
                pin.title = title;
                changed = true;
            }
            if let Some(favicon) = favicons.get(&webview) &&
                pin.favicon.as_ref() != Some(favicon)
            {
                pin.favicon = Some(favicon.clone());
                changed = true;
            }
        }
        if changed {
            self.save();
        }
    }

    fn pin_json(&self, pin: &Pin, window: &ServoShellWindow, active: Option<WebViewId>) -> Value {
        let live = pin.webview.and_then(|id| window.webview_by_id(id));
        let current_url = live.as_ref().and_then(|view| view.url()).map(|url| url.to_string());
        json!({
            "pinId": pin.id,
            "title": pin.custom_title.clone().unwrap_or_else(|| {
                if pin.title.is_empty() { pin.url.clone() } else { pin.title.clone() }
            }),
            "url": pin.url,
            "favicon": pin.favicon,
            "loaded": pin.webview.is_some(),
            // Loading in the background at startup: still shown greyed.
            "preloading": live.as_ref().is_some_and(|view| {
                window.is_untouched_preload(view.id()) &&
                    matches!(view.load_status(), servo::LoadStatus::Started | servo::LoadStatus::HeadParsed)
            }),
            "active": pin.webview.is_some() && pin.webview == active,
            "changed": current_url.is_some_and(|url| url != pin.url),
            "folder": pin.folder,
        })
    }

    /// Essentials, for the grid at the top of the sidebar.
    pub(crate) fn essentials_json(&self, window: &ServoShellWindow, active: Option<WebViewId>) -> Value {
        self.pins
            .iter()
            .filter(|pin| pin.space == ESSENTIALS)
            .map(|pin| self.pin_json(pin, window, active))
            .collect()
    }

    /// The current space's pinned section: folders (with their tabs), then loose pins.
    pub(crate) fn pinned_json(&self, window: &ServoShellWindow, active: Option<WebViewId>) -> Value {
        let folders: Vec<Value> = self
            .folders
            .iter()
            .filter(|folder| self.in_view(folder.space))
            .map(|folder| {
                let pins: Vec<Value> = self
                    .pins
                    .iter()
                    .filter(|pin| pin.space == folder.space && pin.folder == Some(folder.id))
                    .map(|pin| self.pin_json(pin, window, active))
                    .collect();
                json!({ "folderId": folder.id, "name": folder.name, "collapsed": folder.collapsed, "pins": pins })
            })
            .collect();
        let loose: Vec<Value> = self
            .pins
            .iter()
            .filter(|pin| self.in_view(pin.space) && pin.folder.is_none())
            .map(|pin| self.pin_json(pin, window, active))
            .collect();
        json!({ "folders": folders, "pins": loose })
    }
}

/// Unloading tabs to save memory, like Zen's tab unloader.
impl Workspaces {
    /// Unload an open tab: its `WebView` is closed (freeing the page's memory) but the tab
    /// stays in the sidebar. Pinned tabs and Essentials just lose their page, as when the
    /// user unloads them. Never unloads the active tab. Returns whether it was unloaded.
    pub(crate) fn unload_tab(
        &mut self,
        window: &ServoShellWindow,
        webview: WebViewId,
        key: u32,
        favicon: Option<String>,
    ) -> bool {
        if window.active_webview().map(|view| view.id()) == Some(webview) || self.is_unloaded(key) {
            return false;
        }
        let Some(view) = window.webview_by_id(webview) else {
            return false;
        };
        if self.pin_for_webview(webview).is_none() {
            let Some(url) = view.url() else {
                return false;
            };
            self.unloaded.push(UnloadedTab {
                key,
                url: url.to_string(),
                title: view.page_title().unwrap_or_default(),
                custom_title: self.custom_titles.remove(&webview),
                favicon,
                space: self.tab_workspace.get(&webview).copied().unwrap_or(self.current),
                order: self.order_of(webview),
                used: self.used_millis(webview),
            });
        }
        crate::desktop::diagnostics::log(format!("unloading tab {webview:?}"));
        window.queue_user_interface_command(UserInterfaceCommand::CloseWebView(webview));
        true
    }

    /// Load an unloaded tab again (it becomes the active tab).
    pub(crate) fn restore_tab(&mut self, window: &ServoShellWindow, key: u32) {
        let Some(tab) = self.unloaded.iter().find(|tab| tab.key == key) else {
            return;
        };
        let Ok(url) = url::Url::parse(&tab.url) else {
            return;
        };
        crate::desktop::diagnostics::log(format!("reloading unloaded tab {}", tab.url));
        // Show the tab's space (it may be another one, e.g. when cycling tabs).
        if self.enabled {
            self.current = tab.space;
        }
        // Its background load at startup is already on its way: show that one.
        if self.activate_when_preloaded(PreloadTarget::Tab(key)) {
            return;
        }
        self.pending_restore = Some(key);
        self.awaiting_new_tab = true;
        window.queue_user_interface_command(UserInterfaceCommand::NewWebViewWithUrl(url));
    }

    /// Tabs that were loaded again since the last call: (WebView, sidebar key, favicon).
    pub(crate) fn take_restored(&mut self) -> Vec<(WebViewId, u32, Option<String>)> {
        std::mem::take(&mut self.restored)
    }

    pub(crate) fn is_unloaded(&self, key: u32) -> bool {
        self.unloaded.iter().any(|tab| tab.key == key)
    }

    /// Forget an unloaded tab (the user closed it).
    pub(crate) fn close_unloaded(&mut self, key: u32) {
        self.unloaded.retain(|tab| tab.key != key);
    }

    pub(crate) fn rename_unloaded(&mut self, key: u32, title: &str) {
        let title = title.trim();
        if let Some(tab) = self.unloaded.iter_mut().find(|tab| tab.key == key) {
            tab.custom_title = (!title.is_empty()).then(|| title.chars().take(80).collect());
        }
    }

    pub(crate) fn move_unloaded_to_space(&mut self, key: u32, space: u32) {
        if !self.list.iter().any(|workspace| workspace.id == space) {
            return;
        }
        if let Some(tab) = self.unloaded.iter_mut().find(|tab| tab.key == key) {
            tab.space = space;
        }
    }

    /// The page of an unloaded tab, removing it from the list (it's being opened elsewhere,
    /// e.g. in split view).
    pub(crate) fn take_unloaded_url(&mut self, key: u32) -> Option<url::Url> {
        let position = self.unloaded.iter().position(|tab| tab.key == key)?;
        let tab = self.unloaded.remove(position);
        url::Url::parse(&tab.url).ok()
    }

    /// Pin an unloaded tab (to the current space or Essentials); it stays unloaded.
    pub(crate) fn pin_unloaded(&mut self, key: u32, essential: bool) {
        let Some(position) = self.unloaded.iter().position(|tab| tab.key == key) else {
            return;
        };
        let tab = self.unloaded.remove(position);
        let id = self.next_id();
        self.pins.push(Pin {
            id,
            url: tab.url,
            title: tab.title,
            custom_title: tab.custom_title,
            favicon: tab.favicon,
            space: if essential { ESSENTIALS } else { self.current },
            folder: None,
            webview: None,
        });
        self.save();
    }

    /// The sidebar row of an unloaded tab.
    pub(crate) fn unloaded_json(&self, key: u32) -> Option<Value> {
        let tab = self.unloaded.iter().find(|tab| tab.key == key)?;
        let title = tab.custom_title.clone().unwrap_or_else(|| {
            if tab.title.is_empty() { tab.url.clone() } else { tab.title.clone() }
        });
        Some(json!({
            "id": tab.key,
            "title": title,
            "url": tab.url,
            "active": false,
            "loading": self.pending_restore == Some(tab.key),
            "favicon": tab.favicon,
            "loaded": false,
        }))
    }

    /// Open tabs (in any space) that may be unloaded, least recently used first, with how long
    /// ago each was last used. Never the active tab or one in `keep` (e.g. playing media).
    pub(crate) fn unload_candidates(
        &self,
        window: &ServoShellWindow,
        keep: &HashSet<WebViewId>,
    ) -> Vec<(WebViewId, Duration)> {
        let active = window.active_webview().map(|view| view.id());
        let now = Instant::now();
        let mut candidates: Vec<(WebViewId, Duration)> = window
            .webview_ids()
            .into_iter()
            .filter(|id| Some(*id) != active && !keep.contains(id))
            .filter_map(|id| {
                let used = self.last_used.get(&id)?;
                Some((id, now.saturating_duration_since(*used)))
            })
            .collect();
        candidates.sort_by_key(|(_, idle)| std::cmp::Reverse(*idle));
        candidates
    }
}

/// What an import from Zen added.
#[derive(Default)]
pub(crate) struct ImportCounts {
    pub(crate) spaces: usize,
    pub(crate) folders: usize,
    pub(crate) pins: usize,
    pub(crate) essentials: usize,
    pub(crate) tabs: usize,
}

/// The accent colour closest to `color`, or no tint for greys.
fn nearest_color(color: Option<[u8; 3]>) -> usize {
    let Some([r, g, b]) = color else {
        return 0;
    };
    let (max, min) = (r.max(g).max(b), r.min(g).min(b));
    if max.saturating_sub(min) < 40 {
        return 0;
    }
    let distance = |[cr, cg, cb]: [u8; 3]| {
        let d = |a: u8, b: u8| (a as i32 - b as i32).pow(2);
        d(r, cr) + d(g, cg) + d(b, cb)
    };
    COLORS
        .iter()
        .enumerate()
        .filter_map(|(index, color)| Some((index, distance((*color)?))))
        .min_by_key(|(_, distance)| *distance)
        .map_or(0, |(index, _)| index)
}

impl Workspaces {
    /// Add Zen's spaces with their pinned tabs, Essentials and folders (`pins`), and its other
    /// tabs as unloaded tabs (`open_tabs`). Spaces with the same name are merged, and pages
    /// already pinned there aren't pinned twice. `next_key` hands out sidebar ids for tabs.
    pub(crate) fn import_zen(
        &mut self,
        session: &crate::desktop::zen_import::ZenSession,
        pins: bool,
        open_tabs: bool,
        mut next_key: impl FnMut() -> u32,
    ) -> ImportCounts {
        let mut counts = ImportCounts::default();
        // Zen space uuid -> Stillwater space id.
        let mut spaces: HashMap<&str, u32> = HashMap::new();
        for space in &session.spaces {
            let used = session.tabs.iter().any(|tab| {
                tab.space.as_deref() == Some(&space.uuid) &&
                    !tab.essential &&
                    if tab.pinned { pins } else { open_tabs }
            });
            if !used && !pins {
                continue;
            }
            let existing = self
                .list
                .iter()
                .find(|known| known.name.eq_ignore_ascii_case(space.name.trim()))
                .map(|known| known.id);
            let id = existing.unwrap_or_else(|| {
                let id = self.next_id;
                self.next_id += 1;
                let count = self.list.len();
                self.list.push(Workspace {
                    id,
                    name: space.name.trim().chars().take(32).collect(),
                    icon: count % ICONS.len(),
                    color: nearest_color(space.color),
                });
                counts.spaces += 1;
                id
            });
            spaces.insert(&space.uuid, id);
        }
        let current = self.current;
        let space_for = |uuid: &Option<String>| {
            uuid.as_deref()
                .and_then(|uuid| spaces.get(uuid).copied())
                .unwrap_or(current)
        };

        if pins {
            // Zen folder id -> Stillwater folder id (nested folders become top-level ones).
            let mut folders: HashMap<&str, u32> = HashMap::new();
            for folder in &session.folders {
                let used = session
                    .tabs
                    .iter()
                    .any(|tab| tab.pinned && tab.folder.as_deref() == Some(&folder.id));
                if !used {
                    continue;
                }
                let space = space_for(&folder.space);
                let existing = self
                    .folders
                    .iter()
                    .find(|known| known.space == space && known.name == folder.name)
                    .map(|known| known.id);
                let id = existing.unwrap_or_else(|| {
                    let id = self.next_id();
                    self.folders.push(Folder {
                        id,
                        space,
                        name: folder.name.chars().take(40).collect(),
                        collapsed: folder.collapsed,
                    });
                    counts.folders += 1;
                    id
                });
                folders.insert(&folder.id, id);
            }
            for tab in session.tabs.iter().filter(|tab| tab.pinned || tab.essential) {
                let space = if tab.essential { ESSENTIALS } else { space_for(&tab.space) };
                if self.pins.iter().any(|pin| pin.space == space && pin.url == tab.url) {
                    continue;
                }
                let id = self.next_id();
                self.pins.push(Pin {
                    id,
                    url: tab.url.clone(),
                    title: tab.title.clone(),
                    custom_title: None,
                    favicon: None,
                    space,
                    folder: (!tab.essential)
                        .then(|| tab.folder.as_deref().and_then(|folder| folders.get(folder).copied()))
                        .flatten(),
                    webview: None,
                });
                if tab.essential {
                    counts.essentials += 1;
                } else {
                    counts.pins += 1;
                }
            }
        }

        if open_tabs {
            for tab in session.tabs.iter().filter(|tab| !tab.pinned && !tab.essential) {
                let space = space_for(&tab.space);
                if self.unloaded.iter().any(|known| known.space == space && known.url == tab.url) {
                    continue;
                }
                self.next_order += 1;
                self.unloaded.push(UnloadedTab {
                    key: next_key(),
                    url: tab.url.clone(),
                    title: tab.title.clone(),
                    custom_title: None,
                    favicon: None,
                    space,
                    order: self.next_order,
                    used: None,
                });
                counts.tabs += 1;
            }
        }
        self.save();
        counts
    }
}

/// A tab the address bar offers to switch to.
pub(crate) enum TabTarget {
    Tab(TabEntry),
    Pin(u32),
}

impl Workspaces {
    /// Every tab, pinned tab and Essential in every space (but not the active tab), with its
    /// title, URL and space name, for the address bar's "Switch to tab" suggestions.
    pub(crate) fn switch_targets(&self, window: &ServoShellWindow) -> Vec<(TabTarget, String, String, String)> {
        let active = window.active_webview().map(|webview| webview.id());
        let space_name = |space: u32| {
            if space == ESSENTIALS {
                return "Essentials".to_owned();
            }
            // One list of tabs: there's no space to name.
            if !self.enabled {
                return String::new();
            }
            self.list
                .iter()
                .find(|workspace| workspace.id == space)
                .map(|workspace| workspace.name.clone())
                .unwrap_or_default()
        };
        let mut targets = Vec::new();
        for pin in &self.pins {
            if pin.webview.is_some() && pin.webview == active {
                continue;
            }
            let title = pin.custom_title.clone().unwrap_or_else(|| pin.title.clone());
            targets.push((TabTarget::Pin(pin.id), title, pin.url.clone(), space_name(pin.space)));
        }
        for (id, webview) in window.webviews() {
            if Some(id) == active || self.pin_for_webview(id).is_some() {
                continue;
            }
            let Some(space) = self.tab_workspace.get(&id) else {
                continue;
            };
            let title = self
                .custom_titles
                .get(&id)
                .cloned()
                .or_else(|| webview.page_title())
                .unwrap_or_default();
            let url = webview.url().map(|url| url.to_string()).unwrap_or_default();
            targets.push((TabTarget::Tab(TabEntry::Live(id)), title, url, space_name(*space)));
        }
        for tab in &self.unloaded {
            let title = tab.custom_title.clone().unwrap_or_else(|| tab.title.clone());
            targets.push((TabTarget::Tab(TabEntry::Unloaded(tab.key)), title, tab.url.clone(), space_name(tab.space)));
        }
        targets
    }
}
