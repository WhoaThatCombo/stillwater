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

pub(crate) struct Workspaces {
    list: Vec<Workspace>,
    current: u32,
    next_id: u32,
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
        let path = crate::prefs::default_config_dir().map(|dir| dir.join("workspaces.json"));
        let list: Vec<Workspace> = path
            .as_ref()
            .and_then(|path| std::fs::read_to_string(path).ok())
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .and_then(|value| {
                value
                    .as_array()
                    .map(|items| items.iter().filter_map(Workspace::from_json).collect())
            })
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
        Self {
            current: list[0].id,
            list,
            next_id,
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
        let result = path
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|_| std::fs::write(path, Value::from(list).to_string()));
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
        for id in &ids {
            if !self.tab_workspace.contains_key(id) {
                self.tab_workspace.insert(*id, self.current);
                self.awaiting_new_tab = false;
            }
        }
        if let Some(active) = window.active_webview().map(|webview| webview.id()) {
            let workspace = self.tab_workspace[&active];
            if workspace != self.current && !self.awaiting_new_tab {
                self.current = workspace;
            }
            if workspace == self.current {
                self.last_active.insert(workspace, active);
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
