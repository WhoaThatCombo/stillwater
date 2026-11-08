/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! The sidebar's side of session restore, reopening closed tabs and drag and drop
//! (`session-*` commands from `session.js`).

use serde_json::{Value, json};

use super::Sidebar;
use crate::desktop::workspaces::{TabEntry, Workspaces};
use crate::window::ServoShellWindow;

/// Load the spaces and pins, plus the previous session's tabs (as unloaded tabs). Returns
/// them with the last sidebar tab id used.
pub(super) fn load_workspaces() -> (Workspaces, u32) {
    let mut workspaces = Workspaces::load();
    let mut next_tab_id = 1;
    if let Some(pending) = crate::desktop::session::take_pending() {
        workspaces.restore_session(&pending, || {
            next_tab_id += 1;
            next_tab_id
        });
    }
    (workspaces, next_tab_id)
}

impl Sidebar {
    fn new_tab_key(&mut self) -> u32 {
        self.next_tab_id += 1;
        self.next_tab_id
    }

    /// An open or unloaded ordinary tab, by its sidebar id.
    fn tab_entry(&self, window: &ServoShellWindow, tab: &str) -> Option<TabEntry> {
        self.webview_for_tab(window, tab)
            .map(TabEntry::Live)
            .or_else(|| self.unloaded_tab(tab).map(TabEntry::Unloaded))
    }

    /// Remember a tab the user is closing, so Ctrl+Shift+T can bring it back.
    pub(super) fn remember_closed(&mut self, window: &ServoShellWindow, entry: TabEntry) {
        let favicon = match entry {
            TabEntry::Live(id) => self.favicons.get(&id).cloned(),
            TabEntry::Unloaded(_) => None,
        };
        self.workspaces.remember_closed(window, entry, favicon);
    }

    /// Handle a `session-*` command. Arguments are tab-separated.
    pub(super) fn session_command(
        &mut self,
        window: &ServoShellWindow,
        command: &str,
        argument: &str,
    ) {
        let parts: Vec<&str> = argument.split('\t').collect();
        let part = |index: usize| parts.get(index).copied().unwrap_or("");
        let number = |index: usize| part(index).parse::<u32>().ok();
        match command {
            "session-reopen" => {
                let key = self.new_tab_key();
                self.workspaces.reopen_closed(window, key);
            },
            "session-startup" => self.settings.borrow_mut().set_startup(argument),
            // Tests: write the session file now.
            "session-flush" => {
                self.save_session(window);
                if let Some(writer) = &self.session {
                    writer.flush();
                }
                crate::desktop::diagnostics::log("session: flushed");
            },
            // `<tab>\t<tab to put it before, or empty for the end>`
            "session-move-tab" => {
                if let Some(entry) = self.tab_entry(window, part(0)) {
                    let before = self.tab_entry(window, part(1));
                    self.workspaces.move_tab_before(window, entry, before);
                }
            },
            // `<tab>\t<space>`
            "session-tab-space" => {
                if let (Some(entry), Some(space)) = (self.tab_entry(window, part(0)), number(1)) {
                    self.workspaces.move_entry_to_space(window, entry, space);
                }
            },
            // `<tab or pin>\t<e = Essentials, p = pinned>\t<folder or empty>\t<pin to put it
            // before, or empty for the end>`
            "session-pin-tab" | "session-place-pin" => {
                let (essential, folder, before) = (part(1) == "e", number(2), number(3));
                if command == "session-place-pin" {
                    if let Some(pin) = number(0) {
                        self.workspaces.place_pin(pin, essential, folder, before);
                    }
                } else if let Some(entry) = self.tab_entry(window, part(0)) {
                    self.workspaces
                        .pin_entry_at(window, entry, essential, folder, before);
                }
            },
            // `<pin>\t<tab to put it before, or empty for the end>`
            "session-unpin-tab" => {
                if let Some(pin) = number(0) {
                    let before = self.tab_entry(window, part(1));
                    let key = self.new_tab_key();
                    self.workspaces.unpin_to_tabs(window, pin, before, key);
                }
            },
            // `<pin>\t<space>`
            "session-pin-space" => {
                if let (Some(pin), Some(space)) = (number(0), number(1)) {
                    self.workspaces.move_pin_to_space(window, pin, space);
                }
            },
            _ => log::warn!("unknown sidebar message: {command}"),
        }
        self.last_pushed_state = None;
    }

    /// What the page needs to know: whether there are closed tabs to reopen.
    pub(super) fn session_state(&self) -> Value {
        json!({ "closed": self.workspaces.closed_count() })
    }

    /// Hand the session to the writer if it changed (called whenever the sidebar updates).
    pub(super) fn save_session(&mut self, window: &ServoShellWindow) {
        let Some(writer) = &mut self.session else {
            return;
        };
        let Some(signature) = self.workspaces.session_json(window, &self.favicons, false) else {
            return;
        };
        if writer.changed(signature.to_string())
            && let Some(session) = self.workspaces.session_json(window, &self.favicons, true)
        {
            writer.save(session.to_string());
        }
    }
}
