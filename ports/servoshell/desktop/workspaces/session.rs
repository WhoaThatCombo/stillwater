/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Tabs surviving and moving: saving and restoring the session's tabs, reopening closed tabs,
//! and the sidebar's drag and drop (reordering tabs and pins, pinning, moving between spaces).

use std::collections::HashMap;

use serde_json::{Value, json};
use servo::WebViewId;

use super::{ESSENTIALS, TabEntry, UnloadedTab, Workspaces};
use crate::desktop::session::PendingSession;
use crate::window::ServoShellWindow;

/// How many closed tabs Ctrl+Shift+T can bring back.
const MAX_CLOSED: usize = 25;
/// Favicons bigger than this aren't saved with the session.
const MAX_SAVED_FAVICON: usize = 16 * 1024;

impl Workspaces {
    /// Ordinary (unpinned) tabs of `space`, open or unloaded, in sidebar order.
    fn regular_entries_in(&self, window: &ServoShellWindow, space: u32) -> Vec<TabEntry> {
        self.entries_in(window, space)
            .into_iter()
            .filter(|entry| match entry {
                TabEntry::Live(id) => self.pin_for_webview(*id).is_none(),
                TabEntry::Unloaded(_) => true,
            })
            .collect()
    }

    fn space_exists(&self, space: u32) -> bool {
        self.list.iter().any(|workspace| workspace.id == space)
    }

    /// The session to save: every space's ordinary tabs, which one is active, the current
    /// space. `None` while the window has no tabs (it is closing). With `full` false, favicons
    /// are replaced by their length, which is enough to tell whether anything changed.
    pub(crate) fn session_json(
        &self,
        window: &ServoShellWindow,
        favicons: &HashMap<WebViewId, String>,
        full: bool,
    ) -> Option<Value> {
        if window.webview_ids().is_empty() {
            return None;
        }
        let favicon_json = |favicon: Option<&String>| match favicon {
            Some(favicon) if favicon.len() <= MAX_SAVED_FAVICON => {
                if full {
                    json!(favicon)
                } else {
                    json!(favicon.len())
                }
            },
            _ => Value::Null,
        };
        let mut spaces = Vec::with_capacity(self.list.len());
        for workspace in &self.list {
            let mut kept = Vec::new();
            let mut tabs = Vec::new();
            for entry in self.regular_entries_in(window, workspace.id) {
                let tab = match entry {
                    TabEntry::Live(id) => {
                        let Some(view) = window.webview_by_id(id) else {
                            continue;
                        };
                        let Some(url) = view.url() else {
                            continue;
                        };
                        json!({
                            "url": url.to_string(),
                            "title": view.page_title().unwrap_or_default(),
                            "customTitle": self.custom_titles.get(&id),
                            "favicon": favicon_json(favicons.get(&id)),
                        })
                    },
                    TabEntry::Unloaded(key) => {
                        let Some(tab) = self.unloaded.iter().find(|tab| tab.key == key) else {
                            continue;
                        };
                        json!({
                            "url": tab.url,
                            "title": tab.title,
                            "customTitle": tab.custom_title,
                            "favicon": favicon_json(tab.favicon.as_ref()),
                        })
                    },
                };
                kept.push(entry);
                tabs.push(tab);
            }
            // The tab last shown in this space; for a space not visited since the last launch,
            // the one that was active then.
            let live = self
                .last_active
                .get(&workspace.id)
                .map(|id| TabEntry::Live(*id));
            let restored = self
                .session_active
                .get(&workspace.id)
                .map(|key| TabEntry::Unloaded(*key));
            let active = live
                .and_then(|entry| kept.iter().position(|other| *other == entry))
                .or_else(|| {
                    restored.and_then(|entry| kept.iter().position(|other| *other == entry))
                });
            spaces.push(json!({ "id": workspace.id, "active": active, "tabs": tabs }));
        }
        let active = window.active_webview();
        let active_pin = active
            .as_ref()
            .and_then(|view| self.pin_for_webview(view.id()))
            .map(|pin| pin.id);
        Some(json!({
            "version": 1,
            "current": self.current,
            "activeUrl": active.and_then(|view| view.url()).map(|url| url.to_string()),
            "activePin": active_pin,
            "spaces": spaces,
        }))
    }

    /// Add the previous session's tabs as unloaded tabs. If Stillwater's first tab is the
    /// session's active tab, that tab takes the active tab's place (or its pin's).
    pub(crate) fn restore_session(
        &mut self,
        session: &PendingSession,
        mut next_key: impl FnMut() -> u32,
    ) {
        let data = &session.data;
        if let Some(current) = data["current"]
            .as_u64()
            .map(|id| id as u32)
            .filter(|id| self.space_exists(*id))
        {
            self.current = current;
        }
        let mut first_tab = None;
        for space in data["spaces"].as_array().into_iter().flatten() {
            let Some(id) = space["id"]
                .as_u64()
                .map(|id| id as u32)
                .filter(|id| self.space_exists(*id))
            else {
                continue;
            };
            let active = space["active"].as_u64().map(|index| index as usize);
            for (index, tab) in space["tabs"].as_array().into_iter().flatten().enumerate() {
                let Some(url) = tab["url"]
                    .as_str()
                    .filter(|url| url::Url::parse(url).is_ok())
                else {
                    continue;
                };
                let key = next_key();
                self.next_order += 1;
                self.unloaded.push(UnloadedTab {
                    key,
                    url: url.to_owned(),
                    title: tab["title"].as_str().unwrap_or_default().to_owned(),
                    custom_title: tab["customTitle"].as_str().map(str::to_owned),
                    favicon: tab["favicon"].as_str().map(str::to_owned),
                    space: id,
                    order: self.next_order,
                });
                if active == Some(index) {
                    self.session_active.insert(id, key);
                    if id == self.current {
                        first_tab = Some(key);
                    }
                }
            }
        }
        if !session.first_tab_is_active {
            return;
        }
        let pin = data["activePin"]
            .as_u64()
            .map(|id| id as u32)
            .filter(|id| self.pins.iter().any(|pin| pin.id == *id));
        if let Some(pin) = pin {
            self.pending_pin = Some(pin);
            self.awaiting_new_tab = true;
        } else if let Some(key) = first_tab {
            self.pending_restore = Some(key);
            self.awaiting_new_tab = true;
        }
    }

    /// The tab a space showed when the session was saved, if it hasn't been visited since.
    pub(crate) fn take_session_active(&mut self, space: u32) -> Option<TabEntry> {
        let key = self.session_active.remove(&space)?;
        self.is_unloaded(key).then_some(TabEntry::Unloaded(key))
    }

    /// Remember an ordinary tab the user is closing, for Ctrl+Shift+T. Pins aren't closed
    /// (only unloaded), and empty New Tab pages aren't worth bringing back.
    pub(crate) fn remember_closed(
        &mut self,
        window: &ServoShellWindow,
        entry: TabEntry,
        favicon: Option<String>,
    ) {
        let tab = match entry {
            TabEntry::Live(id) => {
                if self.pin_for_webview(id).is_some() {
                    return;
                }
                let Some(view) = window.webview_by_id(id) else {
                    return;
                };
                let Some(url) = view.url() else {
                    return;
                };
                UnloadedTab {
                    key: 0,
                    url: url.to_string(),
                    title: view.page_title().unwrap_or_default(),
                    custom_title: self.custom_titles.get(&id).cloned(),
                    favicon,
                    space: self.tab_workspace.get(&id).copied().unwrap_or(self.current),
                    order: self.order_of(id),
                }
            },
            TabEntry::Unloaded(key) => {
                let Some(tab) = self.unloaded.iter().find(|tab| tab.key == key) else {
                    return;
                };
                UnloadedTab {
                    key: 0,
                    url: tab.url.clone(),
                    title: tab.title.clone(),
                    custom_title: tab.custom_title.clone(),
                    favicon: tab.favicon.clone(),
                    space: tab.space,
                    order: tab.order,
                }
            },
        };
        if matches!(tab.url.as_str(), "servo:newtab" | "about:blank") {
            return;
        }
        self.closed.push(tab);
        if self.closed.len() > MAX_CLOSED {
            self.closed.remove(0);
        }
    }

    pub(crate) fn closed_count(&self) -> usize {
        self.closed.len()
    }

    /// Bring back the most recently closed tab, in its space and at its old place, as tab
    /// `key`. It loads and becomes the active tab (if another tab is still being loaded, it
    /// comes back unloaded). Returns false if there's nothing to reopen.
    pub(crate) fn reopen_closed(&mut self, window: &ServoShellWindow, key: u32) -> bool {
        let Some(mut tab) = self.closed.pop() else {
            return false;
        };
        if !self.space_exists(tab.space) {
            tab.space = self.current;
        }
        tab.key = key;
        // Its old place, unless another tab took that position since (then at the end).
        let taken = tab.order == u64::MAX
            || self.unloaded.iter().any(|other| other.order == tab.order)
            || self.tab_order.values().any(|order| *order == tab.order);
        if taken {
            self.next_order += 1;
            tab.order = self.next_order;
        }
        crate::desktop::diagnostics::log(format!("reopening closed tab {}", tab.url));
        let busy = self.pending_restore.is_some();
        self.unloaded.push(tab);
        if !busy {
            self.restore_tab(window, key);
        }
        true
    }

    /// Give `entries` (in this order) consecutive places in their tab lists.
    fn renumber(&mut self, entries: &[TabEntry]) {
        for entry in entries {
            self.next_order += 1;
            let order = self.next_order;
            match entry {
                TabEntry::Live(id) => {
                    self.tab_order.insert(*id, order);
                },
                TabEntry::Unloaded(key) => {
                    if let Some(tab) = self.unloaded.iter_mut().find(|tab| tab.key == *key) {
                        tab.order = order;
                    }
                },
            }
        }
    }

    /// Move an ordinary tab of the current space before `before` (`None` = to the end).
    pub(crate) fn move_tab_before(
        &mut self,
        window: &ServoShellWindow,
        entry: TabEntry,
        before: Option<TabEntry>,
    ) {
        let mut entries = self.regular_entries(window);
        let Some(from) = entries.iter().position(|other| *other == entry) else {
            return;
        };
        entries.remove(from);
        let to = before
            .and_then(|before| entries.iter().position(|other| *other == before))
            .unwrap_or(entries.len());
        entries.insert(to, entry);
        self.renumber(&entries);
    }

    /// Move an ordinary tab to the end of another space's list.
    pub(crate) fn move_entry_to_space(
        &mut self,
        window: &ServoShellWindow,
        entry: TabEntry,
        space: u32,
    ) {
        if !self.space_exists(space) || space == self.current {
            return;
        }
        match entry {
            TabEntry::Live(id) => self.move_tab_to_space(window, id, space),
            TabEntry::Unloaded(key) => self.move_unloaded_to_space(key, space),
        }
        self.renumber(&[entry]);
    }

    /// Put a pin in Essentials or in the current space's pinned section (in `folder`, if
    /// given), before pin `before` or at the end of that group.
    pub(crate) fn place_pin(
        &mut self,
        pin_id: u32,
        essential: bool,
        folder: Option<u32>,
        before: Option<u32>,
    ) {
        let space = if essential { ESSENTIALS } else { self.current };
        let folder = folder.filter(|_| !essential).filter(|id| {
            self.folders
                .iter()
                .any(|folder| folder.id == *id && folder.space == space)
        });
        let Some(position) = self.pins.iter().position(|pin| pin.id == pin_id) else {
            return;
        };
        let mut pin = self.pins.remove(position);
        pin.space = space;
        pin.folder = folder;
        if let Some(webview) = pin.webview {
            self.tab_workspace.insert(webview, space);
        }
        let in_group = |other: &super::Pin| other.space == space && other.folder == folder;
        let index = before
            .and_then(|before| {
                self.pins
                    .iter()
                    .position(|other| other.id == before && in_group(other))
            })
            .or_else(|| self.pins.iter().rposition(in_group).map(|index| index + 1))
            .unwrap_or(self.pins.len());
        self.pins.insert(index, pin);
        self.save();
    }

    /// Pin an ordinary tab by dropping it in the pinned section or Essentials.
    pub(crate) fn pin_entry_at(
        &mut self,
        window: &ServoShellWindow,
        entry: TabEntry,
        essential: bool,
        folder: Option<u32>,
        before: Option<u32>,
    ) {
        let pin_id = match entry {
            TabEntry::Live(id) => {
                self.pin_tab(window, id, essential);
                self.pin_for_webview(id).map(|pin| pin.id)
            },
            TabEntry::Unloaded(key) => {
                if !self.is_unloaded(key) {
                    return;
                }
                self.pin_unloaded(key, essential);
                self.pins.last().map(|pin| pin.id)
            },
        };
        if let Some(pin_id) = pin_id {
            self.place_pin(pin_id, essential, folder, before);
        }
    }

    /// Turn a pin back into an ordinary tab, placed before `before` (`None` = at the end). An
    /// unloaded pin becomes an unloaded tab with sidebar id `key`.
    pub(crate) fn unpin_to_tabs(
        &mut self,
        window: &ServoShellWindow,
        pin_id: u32,
        before: Option<TabEntry>,
        key: u32,
    ) {
        let Some(position) = self.pins.iter().position(|pin| pin.id == pin_id) else {
            return;
        };
        let entry = match self.pins[position].webview {
            Some(webview) => {
                self.unpin(pin_id);
                TabEntry::Live(webview)
            },
            None => {
                let pin = self.pins.remove(position);
                self.save();
                self.next_order += 1;
                self.unloaded.push(UnloadedTab {
                    key,
                    url: pin.url,
                    title: pin.title,
                    custom_title: pin.custom_title,
                    favicon: pin.favicon,
                    space: self.current,
                    order: self.next_order,
                });
                TabEntry::Unloaded(key)
            },
        };
        self.move_tab_before(window, entry, before);
    }

    /// Move a pinned tab (loaded or not) to another space's pinned section.
    pub(crate) fn move_pin_to_space(&mut self, window: &ServoShellWindow, pin_id: u32, space: u32) {
        if !self.space_exists(space) {
            return;
        }
        let Some(pin) = self.pin_mut(pin_id) else {
            return;
        };
        match pin.webview {
            Some(webview) => self.move_tab_to_space(window, webview, space),
            None => {
                pin.space = space;
                pin.folder = None;
                self.save();
            },
        }
    }
}
