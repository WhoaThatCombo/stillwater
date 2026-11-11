/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Bookmarks: a flat list kept in `<config dir>/stillwater-bookmarks.json`:
//!
//! ```json
//! { "version": 1, "nextId": 3, "bookmarks": [
//!     { "id": 1, "url": "https://example.com/", "title": "Example", "folder": "", "added": 1700000000 }
//! ] }
//! ```
//!
//! `folder` is the name of the folder a bookmark was imported from (shown, not editable);
//! `added` is in seconds since 1970.

use std::path::PathBuf;

use log::warn;
use serde_json::{Value, json};

const BOOKMARKS_FILE: &str = "stillwater-bookmarks.json";

pub(crate) struct Bookmark {
    pub(crate) id: u64,
    pub(crate) url: String,
    pub(crate) title: String,
    pub(crate) folder: String,
    pub(crate) added: u64,
}

impl Bookmark {
    fn from_json(value: &Value) -> Option<Self> {
        Some(Self {
            id: value["id"].as_u64()?,
            url: value["url"].as_str()?.to_owned(),
            title: value["title"].as_str().unwrap_or_default().to_owned(),
            folder: value["folder"].as_str().unwrap_or_default().to_owned(),
            added: value["added"].as_u64().unwrap_or(0),
        })
    }

    pub(crate) fn to_json(&self) -> Value {
        json!({
            "id": self.id, "url": self.url, "title": self.title,
            "folder": self.folder, "added": self.added,
        })
    }
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |time| time.as_secs())
}

fn clean_title(title: &str) -> String {
    title.trim().chars().take(300).collect()
}

#[derive(Default)]
pub(crate) struct Bookmarks {
    list: Vec<Bookmark>,
    next_id: u64,
    path: Option<PathBuf>,
}

impl Bookmarks {
    pub(crate) fn load() -> Self {
        let path = crate::desktop::settings::config_dir().map(|dir| dir.join(BOOKMARKS_FILE));
        let value: Value = path
            .as_ref()
            .and_then(|path| std::fs::read(path).ok())
            .and_then(|data| serde_json::from_slice(&data).ok())
            .unwrap_or(Value::Null);
        let list: Vec<Bookmark> = value["bookmarks"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Bookmark::from_json)
            .collect();
        let next_id = value["nextId"].as_u64().unwrap_or(1).max(
            list.iter()
                .map(|bookmark| bookmark.id + 1)
                .max()
                .unwrap_or(1),
        );
        Self {
            list,
            next_id,
            path,
        }
    }

    /// Read the file again before a change: an incognito window (a process of its own) may
    /// have added bookmarks since, and saving must not drop them.
    fn reload(&mut self) {
        if self.path.is_some() {
            *self = Self::load();
        }
    }

    fn save(&self) {
        let Some(path) = &self.path else {
            return;
        };
        let value = json!({
            "version": 1,
            "nextId": self.next_id,
            "bookmarks": self.list.iter().map(Bookmark::to_json).collect::<Vec<_>>(),
        });
        // Write a new file and swap it in, so a crash can't leave half a file.
        let temporary = path.with_extension("json.tmp");
        let result = path
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|_| std::fs::write(&temporary, value.to_string()))
            .and_then(|_| std::fs::rename(&temporary, path));
        if let Err(error) = result {
            warn!("couldn't save bookmarks: {error}");
        }
    }

    pub(crate) fn find(&self, url: &str) -> Option<&Bookmark> {
        self.list.iter().find(|bookmark| bookmark.url == url)
    }

    fn insert(&mut self, url: &str, title: &str, folder: &str, added: u64) -> bool {
        if self.find(url).is_some() {
            return false;
        }
        let title = clean_title(title);
        self.list.push(Bookmark {
            id: self.next_id,
            url: url.to_owned(),
            title: if title.is_empty() {
                url.to_owned()
            } else {
                title
            },
            folder: folder.to_owned(),
            added,
        });
        self.next_id += 1;
        true
    }

    /// Bookmark a page (unless it already is).
    pub(crate) fn add(&mut self, url: &str, title: &str, folder: &str) {
        self.reload();
        if self.insert(url, title, folder, now()) {
            self.save();
        }
    }

    /// Add bookmarks from another browser: (url, title, folder, added). Pages that are already
    /// bookmarked are skipped. Returns how many were added.
    pub(crate) fn import(&mut self, bookmarks: &[(String, String, String, u64)]) -> usize {
        self.reload();
        let added = bookmarks
            .iter()
            .filter(|(url, title, folder, added)| self.insert(url, title, folder, *added))
            .count();
        if added > 0 {
            self.save();
        }
        added
    }

    pub(crate) fn rename(&mut self, id: u64, title: &str) {
        self.reload();
        let title = clean_title(title);
        if let Some(bookmark) = self.list.iter_mut().find(|bookmark| bookmark.id == id) {
            if !title.is_empty() && bookmark.title != title {
                bookmark.title = title;
                self.save();
            }
        }
    }

    pub(crate) fn remove(&mut self, id: u64) {
        self.reload();
        let count = self.list.len();
        self.list.retain(|bookmark| bookmark.id != id);
        if self.list.len() != count {
            self.save();
        }
    }

    /// Bookmarks whose title or URL contains every word (lowercase), newest first.
    pub(crate) fn search(&self, words: &[String], limit: usize) -> Vec<&Bookmark> {
        self.list
            .iter()
            .rev()
            .filter(|bookmark| {
                let title = bookmark.title.to_lowercase();
                let url = bookmark.url.to_lowercase();
                let address = url.split_once("://").map_or(url.as_str(), |(_, rest)| rest);
                words
                    .iter()
                    .all(|word| title.contains(word.as_str()) || address.contains(word.as_str()))
            })
            .take(limit)
            .collect()
    }

    /// All bookmarks, newest first.
    pub(crate) fn to_json(&self) -> Value {
        self.list.iter().rev().map(Bookmark::to_json).collect()
    }
}
