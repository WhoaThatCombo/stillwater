/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Zen-style workspaces ("spaces"): every tab belongs to exactly one workspace, the sidebar
//! only lists the current workspace's tabs, and each workspace has its own icon and accent
//! colour. Workspace definitions (not their tabs) persist in `<config dir>/workspaces.json`.

use std::collections::HashMap;
use std::path::PathBuf;

use log::warn;
use serde_json::{Value, json};
use servo::{Theme, WebViewId};

use crate::running_app_state::UserInterfaceCommand;
use crate::window::ServoShellWindow;

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
    /// Which workspace each open tab belongs to.
    tab_workspace: HashMap<WebViewId, u32>,
    /// The tab to show again when switching back to a workspace.
    last_active: HashMap<u32, WebViewId>,
    /// A new tab was requested for the current workspace; until it appears, don't follow the
    /// (old) active tab into another workspace.
    awaiting_new_tab: bool,
    path: Option<PathBuf>,
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
            tab_workspace: Default::default(),
            last_active: Default::default(),
            awaiting_new_tab: false,
            path,
        }
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

    /// Tabs belonging to `workspace`, in creation order.
    fn tabs_in(&self, window: &ServoShellWindow, workspace: u32) -> Vec<WebViewId> {
        window
            .webview_ids()
            .into_iter()
            .filter(|id| self.tab_workspace.get(id) == Some(&workspace))
            .collect()
    }

    pub(crate) fn current_tabs(&self, window: &ServoShellWindow) -> Vec<WebViewId> {
        self.tabs_in(window, self.current)
    }

    /// Assign new tabs to the current workspace, forget closed ones, and follow the active tab
    /// if something (e.g. a link opening a popup) activated a tab in another workspace.
    pub(crate) fn sync(&mut self, window: &ServoShellWindow) {
        let ids = window.webview_ids();
        self.tab_workspace.retain(|id, _| ids.contains(id));
        self.last_active.retain(|_, id| ids.contains(id));
        self.custom_titles.retain(|id, _| ids.contains(id));
        // A closed pinned tab is unloaded, not removed.
        for pin in &mut self.pins {
            if pin.webview.is_some_and(|id| !ids.contains(&id)) {
                pin.webview = None;
            }
        }
        for id in &ids {
            if !self.tab_workspace.contains_key(id) {
                // The tab a pinned tab asked for, or an ordinary new tab.
                let pin = self
                    .pending_pin
                    .take()
                    .and_then(|pin_id| self.pins.iter_mut().find(|pin| pin.id == pin_id));
                let space = match pin {
                    Some(pin) => {
                        pin.webview = Some(*id);
                        pin.space
                    },
                    None => self.current,
                };
                self.tab_workspace.insert(*id, space);
                self.awaiting_new_tab = false;
            }
        }
        if let Some(active) = window.active_webview().map(|webview| webview.id()) {
            let workspace = self.tab_workspace[&active];
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
            let siblings: Vec<_> = self
                .tabs_in(window, workspace)
                .into_iter()
                .filter(|other| *other != id)
                .collect();
            match siblings.last() {
                Some(sibling) => window.activate_webview(*sibling),
                None => self.open_tab(window),
            }
        }
        window.queue_user_interface_command(UserInterfaceCommand::CloseWebView(id));
    }

    /// Activate the `index`-th tab of the current workspace (`usize::MAX` = last).
    pub(crate) fn activate_tab_index(&self, window: &ServoShellWindow, index: usize) {
        let tabs = self.current_tabs(window);
        let tab = if index == usize::MAX { tabs.last() } else { tabs.get(index) };
        if let Some(tab) = tab {
            window.activate_webview(*tab);
        }
    }

    /// Activate the next (`delta` = 1) or previous (`delta` = -1) tab in the current workspace.
    pub(crate) fn cycle_tab(&self, window: &ServoShellWindow, delta: isize) {
        let tabs = self.current_tabs(window);
        let Some(active) = window.active_webview().map(|webview| webview.id()) else {
            return;
        };
        if let Some(position) = tabs.iter().position(|id| *id == active) {
            let next = (position as isize + delta).rem_euclid(tabs.len() as isize);
            window.activate_webview(tabs[next as usize]);
        }
    }

    pub(crate) fn switch_to(&mut self, window: &ServoShellWindow, workspace: u32) {
        if workspace == self.current || self.get_mut(workspace).is_none() {
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
            None => self.open_tab(window),
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
        // The space's pinned tabs and folders go with it.
        self.pins.retain(|pin| pin.space != id);
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
                        .map(|[r, g, b]| format!("rgb({r}, {g}, {b})")),
                })
            })
            .collect();
        json!({ "list": list, "current": self.current })
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

    /// The current space's ordinary (unpinned) tabs, in creation order.
    pub(crate) fn regular_tabs(&self, window: &ServoShellWindow) -> Vec<WebViewId> {
        self.current_tabs(window)
            .into_iter()
            .filter(|id| self.pin_for_webview(*id).is_none())
            .collect()
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
        let folder = folder.filter(|id| self.folders.iter().any(|folder| folder.id == *id));
        if let Some(pin) = self.pin_mut(pin_id) &&
            pin.space != ESSENTIALS
        {
            pin.folder = folder;
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
            .filter(|index| self.pins[*index].space == space && self.pins[*index].folder == folder)
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
            .filter(|folder| folder.space == self.current)
            .map(|folder| {
                let pins: Vec<Value> = self
                    .pins
                    .iter()
                    .filter(|pin| pin.space == self.current && pin.folder == Some(folder.id))
                    .map(|pin| self.pin_json(pin, window, active))
                    .collect();
                json!({ "folderId": folder.id, "name": folder.name, "collapsed": folder.collapsed, "pins": pins })
            })
            .collect();
        let loose: Vec<Value> = self
            .pins
            .iter()
            .filter(|pin| pin.space == self.current && pin.folder.is_none())
            .map(|pin| self.pin_json(pin, window, active))
            .collect();
        json!({ "folders": folders, "pins": loose })
    }
}