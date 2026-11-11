/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Browsing history, address-bar suggestions and bookmarks.
//!
//! - Visits to top-level pages are kept in `<config dir>/stillwater-history.sqlite` (WAL mode).
//!   They're written by a background thread in batches, so page loads never wait for the disk.
//!   The main thread reads (the History panel) and makes the rare deletions the user asks for.
//!   Visits older than 90 days are removed at startup.
//! - Address-bar suggestions search an in-memory copy of the pages (URL, title, visit count,
//!   last visit), loaded when the URL bar is first used and kept up to date as pages are
//!   visited, because a full SQL text search over tens of thousands of pages takes too long.
//! - Bookmarks are a flat list in `<config dir>/stillwater-bookmarks.json` (see `bookmarks.rs`).
//!
//! The sidebar asks with `hist-*` / `bm-*` commands; the answers go straight to the page with
//! `window.sidebar.<function>(json)`, so the history itself never goes into the sidebar state.
//! Nothing here is logged except counts and timings.

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use log::warn;
use rusqlite::{Connection, params, params_from_iter};
use serde_json::{Value, json};
use servo::{StorageType, WebView, WebViewId};
use url::Url;

use crate::desktop::bookmarks::Bookmarks;
use crate::desktop::sidebar::Sidebar;
use crate::desktop::zen_import::ZenVisit;
use crate::window::ServoShellWindow;

const HISTORY_FILE: &str = "stillwater-history.sqlite";
const DAY_MS: i64 = 24 * 60 * 60 * 1000;
/// Visits older than this many days are forgotten.
pub(crate) const KEEP_DAYS: i64 = 90;
/// A new URL in the same tab this soon after the last one replaces that visit: client-side
/// redirects and pages that rewrite their URL while loading count once.
const REPLACE_WINDOW: Duration = Duration::from_millis(1000);
/// Rows in the address bar's suggestion list, including the "Search" / "Go to" row.
const MAX_SUGGESTIONS: usize = 8;
/// History panel rows per page.
const PAGE_SIZE: usize = 100;
/// Longer URLs aren't recorded.
const MAX_URL: usize = 4096;

const SCHEMA: &str = "
    PRAGMA journal_mode = WAL;
    PRAGMA synchronous = NORMAL;
    PRAGMA secure_delete = ON;
    CREATE TABLE IF NOT EXISTS places (
        id INTEGER PRIMARY KEY,
        url TEXT NOT NULL UNIQUE,
        title TEXT NOT NULL DEFAULT '',
        visit_count INTEGER NOT NULL DEFAULT 0,
        last_visit INTEGER NOT NULL DEFAULT 0
    );
    CREATE INDEX IF NOT EXISTS places_last_visit ON places (last_visit);
    CREATE TABLE IF NOT EXISTS visits (
        id INTEGER PRIMARY KEY,
        place INTEGER NOT NULL,
        time INTEGER NOT NULL
    );
    CREATE INDEX IF NOT EXISTS visits_time ON visits (time);
    CREATE INDEX IF NOT EXISTS visits_place ON visits (place, time);
";

/// Milliseconds since 1970.
pub(crate) fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |time| time.as_millis() as i64)
}

/// Only web pages and local files are recorded: not `servo:`, `about:`, `data:` and the like.
pub(crate) fn recordable(url: &str) -> bool {
    (url.starts_with("http://") || url.starts_with("https://") || url.starts_with("file:"))
        && url.len() <= MAX_URL
}

fn open(path: &Path) -> rusqlite::Result<Connection> {
    let db = Connection::open(path)?;
    db.busy_timeout(Duration::from_secs(5))?;
    db.execute_batch(SCHEMA)?;
    Ok(db)
}

/// Remove the visits made in `[from, to)` and the pages left without visits.
fn remove_visits(db: &Connection, from: i64, to: i64) -> rusqlite::Result<()> {
    let tx = db.unchecked_transaction()?;
    tx.execute(
        "UPDATE places SET
            visit_count = visit_count -
                (SELECT count(*) FROM visits WHERE place = places.id AND time >= ?1 AND time < ?2),
            last_visit = coalesce(
                (SELECT max(time) FROM visits WHERE place = places.id AND (time < ?1 OR time >= ?2)), 0)
         WHERE id IN (SELECT place FROM visits WHERE time >= ?1 AND time < ?2)",
        params![from, to],
    )?;
    tx.execute(
        "DELETE FROM visits WHERE time >= ?1 AND time < ?2",
        params![from, to],
    )?;
    tx.execute("DELETE FROM places WHERE visit_count <= 0", [])?;
    tx.commit()
}

// ---- Writing visits on a background thread ----

enum Write {
    /// A visit, possibly replacing the visit (URL and time) made just before in the same tab.
    Visit {
        url: String,
        time: i64,
        replaces: Option<(String, i64)>,
    },
    Title {
        url: String,
        title: String,
    },
}

fn writer(path: PathBuf, receiver: Receiver<Write>) {
    let db = match open(&path) {
        Ok(db) => db,
        Err(error) => {
            warn!("history: couldn't open the database: {error}");
            return;
        },
    };
    let started = Instant::now();
    if let Err(error) = remove_visits(&db, 0, now_ms() - KEEP_DAYS * DAY_MS) {
        warn!("history: couldn't remove old visits: {error}");
    }
    crate::desktop::diagnostics::log(format!(
        "history: expired old visits in {} ms",
        started.elapsed().as_millis()
    ));
    while let Ok(first) = receiver.recv() {
        // Everything queued meanwhile goes into the same transaction.
        let batch: Vec<Write> = std::iter::once(first)
            .chain(receiver.try_iter().take(1000))
            .collect();
        if let Err(error) = write_batch(&db, batch) {
            warn!("history: couldn't save visits: {error}");
        }
    }
}

fn write_batch(db: &Connection, batch: Vec<Write>) -> rusqlite::Result<()> {
    let tx = db.unchecked_transaction()?;
    for write in batch {
        match write {
            Write::Visit {
                url,
                time,
                replaces,
            } => {
                if let Some((old_url, old_time)) = replaces {
                    let removed = tx.execute(
                        "DELETE FROM visits WHERE time = ?2 AND place = (SELECT id FROM places WHERE url = ?1)",
                        params![old_url, old_time],
                    )?;
                    if removed > 0 {
                        tx.execute(
                            "UPDATE places SET visit_count = visit_count - 1, last_visit =
                                coalesce((SELECT max(time) FROM visits WHERE place = places.id), 0)
                             WHERE url = ?1",
                            [&old_url],
                        )?;
                        tx.execute(
                            "DELETE FROM places WHERE url = ?1 AND visit_count <= 0",
                            [&old_url],
                        )?;
                    }
                }
                tx.execute(
                    "INSERT INTO places (url, visit_count, last_visit) VALUES (?1, 1, ?2)
                     ON CONFLICT (url) DO UPDATE SET visit_count = visit_count + 1,
                        last_visit = max(last_visit, excluded.last_visit)",
                    params![url, time],
                )?;
                tx.execute(
                    "INSERT INTO visits (place, time) SELECT id, ?2 FROM places WHERE url = ?1",
                    params![url, time],
                )?;
            },
            Write::Title { url, title } => {
                tx.execute(
                    "UPDATE places SET title = ?2 WHERE url = ?1",
                    params![url, title],
                )?;
            },
        }
    }
    tx.commit()
}

// ---- The in-memory copy used for suggestions ----

struct Page {
    url: Box<str>,
    /// What typed text is matched against: the URL without its scheme, a newline and the
    /// title, in lowercase. (The title as written is read from the database for the few pages
    /// shown, to keep this copy small.)
    text: Box<str>,
    visit_count: u32,
    last_visit: i64,
    /// Where the host starts in `text` (after `www.`).
    host_start: u16,
}

impl Page {
    fn new(url: &str, title: &str, visit_count: u32, last_visit: i64) -> Self {
        let address = url.find("://").map_or(url, |index| &url[index + 3..]);
        let host_start = if address.starts_with("www.") { 4 } else { 0 };
        let mut page = Self {
            url: url.into(),
            text: Box::default(),
            visit_count,
            last_visit,
            host_start,
        };
        page.set_title(title);
        page
    }

    fn address(&self) -> &str {
        self.url
            .find("://")
            .map_or(&*self.url, |index| &self.url[index + 3..])
    }

    fn set_title(&mut self, title: &str) {
        self.text = format!("{}\n{}", self.address(), title)
            .to_lowercase()
            .into_boxed_str();
    }

    /// The title, lowercase (used when the database doesn't have it yet).
    fn lowercase_title(&self) -> &str {
        self.text.split_once('\n').map_or("", |(_, title)| title)
    }
}

/// Firefox-style frecency: visit count weighted by how recent the last visit was.
fn frecency(visit_count: u32, last_visit: i64, now: i64) -> u64 {
    let age = now - last_visit;
    let weight = if age < 4 * DAY_MS {
        100
    } else if age < 14 * DAY_MS {
        70
    } else if age < 31 * DAY_MS {
        50
    } else if age < 90 * DAY_MS {
        30
    } else {
        10
    };
    visit_count as u64 * weight
}

/// Case-insensitive substring search; `needle` is already lowercase.
fn contains(haystack: &str, needle: &str) -> bool {
    if haystack.is_ascii() {
        let needle = needle.as_bytes();
        haystack
            .as_bytes()
            .windows(needle.len().max(1))
            .any(|window| window.eq_ignore_ascii_case(needle))
    } else {
        haystack.to_lowercase().contains(needle)
    }
}

fn url_hash(url: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    url.hash(&mut hasher);
    hasher.finish()
}

#[derive(Default)]
struct Index {
    pages: Vec<Page>,
    /// URL hash -> positions in `pages`.
    by_url: HashMap<u64, Vec<u32>>,
}

impl Index {
    fn load(db: &Connection) -> rusqlite::Result<Self> {
        let mut index = Index::default();
        let mut statement = db.prepare("SELECT url, title, visit_count, last_visit FROM places")?;
        let rows = statement.query_map([], |row| {
            Ok(Page::new(
                &row.get::<_, String>(0)?,
                &row.get::<_, String>(1)?,
                row.get(2)?,
                row.get(3)?,
            ))
        })?;
        for page in rows.flatten() {
            index.insert(page);
        }
        Ok(index)
    }

    fn insert(&mut self, page: Page) {
        let position = self.pages.len() as u32;
        self.by_url
            .entry(url_hash(&page.url))
            .or_default()
            .push(position);
        self.pages.push(page);
    }

    fn find(&mut self, url: &str) -> Option<&mut Page> {
        let positions = self.by_url.get(&url_hash(url))?;
        let position = positions
            .iter()
            .find(|position| &*self.pages[**position as usize].url == url)?;
        self.pages.get_mut(*position as usize)
    }

    fn visit(&mut self, url: &str, time: i64) {
        match self.find(url) {
            Some(page) => {
                page.visit_count += 1;
                page.last_visit = page.last_visit.max(time);
            },
            None => self.insert(Page::new(url, "", 1, time)),
        }
    }

    /// The best pages matching every word (lowercase), pages whose host starts with the first
    /// word first, then by frecency.
    fn search(&self, words: &[String], limit: usize) -> Vec<&Page> {
        let now = now_ms();
        let prefix = words.first().map(|word| {
            let word = word
                .strip_prefix("https://")
                .or_else(|| word.strip_prefix("http://"))
                .unwrap_or(word);
            word.strip_prefix("www.").unwrap_or(word).to_owned()
        });
        let mut matches: Vec<(bool, u64, usize, &Page)> = self
            .pages
            .iter()
            .filter(|page| page.visit_count > 0)
            .filter(|page| words.iter().all(|word| page.text.contains(word.as_str())))
            .map(|page| {
                let host_prefix = prefix.as_deref().is_some_and(|prefix| {
                    !prefix.is_empty() && page.text[page.host_start as usize..].starts_with(prefix)
                });
                (
                    host_prefix,
                    frecency(page.visit_count, page.last_visit, now),
                    page.url.len(),
                    page,
                )
            })
            .collect();
        let order = |a: &(bool, u64, usize, &Page), b: &(bool, u64, usize, &Page)| {
            b.0.cmp(&a.0).then(b.1.cmp(&a.1)).then(a.2.cmp(&b.2))
        };
        if matches.len() > limit {
            matches.select_nth_unstable_by(limit, order);
            matches.truncate(limit);
        }
        matches.sort_by(order);
        matches.into_iter().map(|(.., page)| page).collect()
    }
}

/// The last visit recorded in a tab.
struct LastVisit {
    url: String,
    time: i64,
    when: Instant,
    title: String,
}

pub(crate) struct History {
    path: Option<PathBuf>,
    /// The main thread's connection, opened on first use.
    db: Option<Connection>,
    writer: Option<Sender<Write>>,
    writer_thread: Option<std::thread::JoinHandle<()>>,
    last: HashMap<WebViewId, LastVisit>,
    /// The in-memory copy for suggestions, loaded on first use.
    index: Option<Index>,
    pub(crate) bookmarks: Bookmarks,
}

impl Drop for History {
    /// Let the writer save the visits still queued before Stillwater exits.
    fn drop(&mut self) {
        self.writer = None;
        if let Some(thread) = self.writer_thread.take() {
            let _ = thread.join();
        }
    }
}

impl History {
    pub(crate) fn new() -> Self {
        // Incognito: no history at all (not even the normal one, which would mean opening it).
        let path = crate::desktop::settings::config_dir()
            .filter(|_| !crate::desktop::incognito::active())
            .map(|dir| {
                let _ = std::fs::create_dir_all(&dir);
                dir.join(HISTORY_FILE)
            });
        let (writer, writer_thread) = path
            .clone()
            .and_then(|path| {
                let (sender, receiver) = channel();
                std::thread::Builder::new()
                    .name("History".into())
                    .spawn(move || writer(path, receiver))
                    .ok()
                    .map(|thread| (sender, thread))
            })
            .unzip();
        Self {
            path,
            db: None,
            writer,
            writer_thread,
            last: HashMap::new(),
            index: None,
            bookmarks: Bookmarks::load(),
        }
    }

    fn db(&mut self) -> Option<&Connection> {
        if self.db.is_none() {
            let path = self.path.as_ref()?;
            match open(path) {
                Ok(db) => self.db = Some(db),
                Err(error) => {
                    warn!("history: couldn't open the database: {error}");
                    self.path = None;
                },
            }
        }
        self.db.as_ref()
    }

    fn index(&mut self) -> Option<&Index> {
        if self.index.is_none() {
            let started = Instant::now();
            let index = Index::load(self.db()?).ok()?;
            crate::desktop::diagnostics::log(format!(
                "history: loaded {} pages for suggestions in {} ms",
                index.pages.len(),
                started.elapsed().as_millis()
            ));
            self.index = Some(index);
        }
        self.index.as_ref()
    }

    /// Load the suggestion index now, so the first suggestion doesn't wait for it.
    pub(crate) fn warm(&mut self) {
        self.index();
    }

    /// A tab's page changed: record a visit.
    pub(crate) fn on_url_changed(&mut self, webview: WebViewId, url: &Url) {
        let Some(writer) = &self.writer else {
            return;
        };
        let url = url.as_str();
        if self.last.get(&webview).is_some_and(|last| last.url == url) {
            return;
        }
        if !recordable(url) {
            self.last.remove(&webview);
            return;
        }
        let previous = self.last.remove(&webview);
        let time = now_ms().max(previous.as_ref().map_or(0, |last| last.time + 1));
        let replaces = previous
            .filter(|last| last.when.elapsed() < REPLACE_WINDOW)
            .map(|last| (last.url, last.time));
        if let Some(index) = &mut self.index {
            if let Some((old, _)) = &replaces
                && let Some(page) = index.find(old)
            {
                page.visit_count = page.visit_count.saturating_sub(1);
            }
            index.visit(url, time);
        }
        let _ = writer.send(Write::Visit {
            url: url.to_owned(),
            time,
            replaces,
        });
        // Closed tabs aren't reported here; don't let the map grow without bound.
        if self.last.len() > 500 {
            self.last.clear();
        }
        self.last.insert(
            webview,
            LastVisit {
                url: url.to_owned(),
                time,
                when: Instant::now(),
                title: String::new(),
            },
        );
    }

    /// A tab's title changed or its page finished loading: save the title of the visited page.
    pub(crate) fn on_title(&mut self, webview: &WebView) {
        let (Some(url), Some(title)) = (webview.url(), webview.page_title()) else {
            return;
        };
        let title: String = title.trim().chars().take(500).collect();
        let Some(last) = self.last.get_mut(&webview.id()) else {
            return;
        };
        if last.url != url.as_str() || title.is_empty() || last.title == title {
            return;
        }
        last.title = title.clone();
        if let Some(index) = &mut self.index
            && let Some(page) = index.find(url.as_str())
        {
            page.set_title(&title);
        }
        if let Some(writer) = &self.writer {
            let _ = writer.send(Write::Title {
                url: url.to_string(),
                title,
            });
        }
    }

    /// Pages matching `words` (lowercase) for the address bar.
    fn suggest(&mut self, words: &[String], limit: usize) -> Vec<(String, String)> {
        let Some(index) = self.index() else {
            return Vec::new();
        };
        let pages: Vec<(String, String)> = index
            .search(words, limit)
            .into_iter()
            .map(|page| (page.url.to_string(), page.lowercase_title().to_owned()))
            .collect();
        // The titles as written, from the database.
        let Some(db) = self.db() else {
            return pages;
        };
        let Ok(mut statement) = db.prepare_cached("SELECT title FROM places WHERE url = ?1") else {
            return pages;
        };
        pages
            .into_iter()
            .map(|(url, lowercase)| {
                let title = statement
                    .query_row([&url], |row| row.get::<_, String>(0))
                    .ok()
                    .filter(|title| !title.is_empty())
                    .unwrap_or(lowercase);
                (url, title)
            })
            .collect()
    }

    /// One page of the History panel: pages last visited before `before` (ms), newest first,
    /// optionally only those matching `words`. Returns the rows and whether there are more.
    fn list(&mut self, before: i64, words: &[String]) -> (Vec<Value>, bool) {
        let Some(db) = self.db() else {
            return (Vec::new(), false);
        };
        let mut sql = String::from(
            "SELECT id, url, title, visit_count, last_visit FROM places WHERE last_visit < ?1",
        );
        for index in 0..words.len() {
            sql.push_str(&format!(
                " AND (url LIKE ?{0} ESCAPE '^' OR title LIKE ?{0} ESCAPE '^')",
                index + 2
            ));
        }
        sql.push_str(&format!(
            " ORDER BY last_visit DESC LIMIT {}",
            PAGE_SIZE + 1
        ));
        let mut arguments: Vec<rusqlite::types::Value> = vec![before.into()];
        for word in words {
            let escaped = word
                .replace('^', "^^")
                .replace('%', "^%")
                .replace('_', "^_");
            arguments.push(format!("%{escaped}%").into());
        }
        let rows: Vec<Value> = db
            .prepare(&sql)
            .and_then(|mut statement| {
                statement
                    .query_map(params_from_iter(arguments), |row| {
                        Ok(json!({
                            "id": row.get::<_, i64>(0)?,
                            "url": row.get::<_, String>(1)?,
                            "title": row.get::<_, String>(2)?,
                            "visits": row.get::<_, i64>(3)?,
                            "time": row.get::<_, i64>(4)?,
                        }))
                    })?
                    .collect()
            })
            .unwrap_or_else(|error| {
                warn!("history: couldn't list pages: {error}");
                Vec::new()
            });
        let more = rows.len() > PAGE_SIZE;
        (rows.into_iter().take(PAGE_SIZE).collect(), more)
    }

    /// After deleting history, the tabs' last visits are gone: don't replace them, but still
    /// don't count a reload of the page a tab shows as a new visit.
    fn forget_recent_visits(&mut self) {
        for last in self.last.values_mut() {
            last.when = last.when.checked_sub(REPLACE_WINDOW).unwrap_or(last.when);
        }
    }

    /// Forget one page and all its visits.
    fn delete_page(&mut self, id: i64) {
        let Some(db) = self.db() else {
            return;
        };
        let result = db
            .execute("DELETE FROM visits WHERE place = ?1", [id])
            .and_then(|_| db.execute("DELETE FROM places WHERE id = ?1", [id]));
        if let Err(error) = result {
            warn!("history: couldn't delete a page: {error}");
        }
        self.index = None;
        self.forget_recent_visits();
    }

    /// Forget the visits made since `since` (ms; 0 = everything).
    fn clear(&mut self, since: i64) -> bool {
        let Some(db) = self.db() else {
            return false;
        };
        let result = if since <= 0 {
            db.execute_batch("DELETE FROM visits; DELETE FROM places;")
        } else {
            remove_visits(db, since, i64::MAX)
        };
        // Don't leave the deleted pages in the write-ahead log.
        let _ = db.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
        self.index = None;
        self.forget_recent_visits();
        if let Err(error) = &result {
            warn!("history: couldn't clear history: {error}");
        }
        result.is_ok()
    }

    /// Add visits imported from another browser (visits already present are skipped).
    /// Returns how many visits were added.
    pub(crate) fn import(&mut self, visits: &[ZenVisit]) -> rusqlite::Result<usize> {
        let cutoff = now_ms() - KEEP_DAYS * DAY_MS;
        let Some(db) = self.db() else {
            return Ok(0);
        };
        let tx = db.unchecked_transaction()?;
        let mut added = 0;
        {
            let mut place = tx.prepare(
                "INSERT INTO places (url, title) VALUES (?1, ?2)
                 ON CONFLICT (url) DO UPDATE SET title = CASE WHEN title = '' THEN excluded.title ELSE title END
                 RETURNING id",
            )?;
            let mut visit = tx.prepare(
                "INSERT INTO visits (place, time) SELECT ?1, ?2
                 WHERE NOT EXISTS (SELECT 1 FROM visits WHERE place = ?1 AND time = ?2)",
            )?;
            let mut ids: HashMap<&str, i64> = HashMap::new();
            for item in visits {
                if item.time < cutoff || !recordable(&item.url) {
                    continue;
                }
                let id = match ids.get(item.url.as_str()) {
                    Some(id) => *id,
                    None => {
                        let id: i64 =
                            place.query_row(params![item.url, item.title], |row| row.get(0))?;
                        ids.insert(&item.url, id);
                        id
                    },
                };
                added += visit.execute(params![id, item.time])?;
            }
        }
        tx.execute(
            "UPDATE places SET
                visit_count = (SELECT count(*) FROM visits WHERE place = places.id),
                last_visit = coalesce((SELECT max(time) FROM visits WHERE place = places.id), 0)",
            [],
        )?;
        tx.execute("DELETE FROM places WHERE visit_count <= 0", [])?;
        tx.commit()?;
        self.index = None;
        Ok(added)
    }

    /// What the sidebar's star button needs: whether the page in the URL bar is bookmarked.
    pub(crate) fn sidebar_json(&self, url: Option<&Url>) -> Value {
        let url = url.map(Url::as_str).filter(|url| recordable(url));
        json!({
            "canBookmark": url.is_some(),
            "starred": url.and_then(|url| self.bookmarks.find(url)).map(|bookmark| bookmark.to_json()),
        })
    }
}

/// Clear cookies, local and session storage and the HTTP cache, for every site.
fn clear_site_data(servo: &servo::Servo) {
    let manager = servo.site_data_manager();
    manager.clear_cookies(None);
    let storage = StorageType::Local | StorageType::Session;
    let sites: Vec<String> = manager
        .site_data(storage)
        .iter()
        .map(servo::SiteData::name)
        .collect();
    let sites: Vec<&str> = sites.iter().map(String::as_str).collect();
    manager.clear_site_data(&sites, storage);
    servo.network_manager().clear_cache();
}

/// Lowercase words of what was typed.
fn words(text: &str) -> Vec<String> {
    text.split_whitespace()
        .map(str::to_lowercase)
        .take(8)
        .collect()
}

/// The address bar's suggestions for `text`: the typed text itself (go to it, or search for
/// it), then matching tabs in any space, pinned tabs and Essentials, bookmarks and history.
pub(crate) fn suggestions(history: &mut History, tabs: Vec<Value>, text: &str) -> Vec<Value> {
    let text = text.trim();
    let words = words(text);
    if words.is_empty() {
        return Vec::new();
    }
    let template = crate::desktop::settings::search_template();
    let query: String = url::form_urlencoded::byte_serialize(text.as_bytes()).collect();
    let search_url = template.replace("%s", &query);
    let target =
        crate::parser::location_bar_input_to_url(text, &template).map(|url| url.to_string());
    let mut rows = Vec::new();
    // The first row is what Enter does with nothing selected.
    let search_prefix = template.split("%s").next().unwrap_or_default();
    let go_to = target.filter(|target| !target.starts_with(search_prefix));
    if let Some(target) = &go_to {
        rows.push(json!({ "kind": "visit", "url": target, "title": text }));
    } else {
        rows.push(json!({ "kind": "search", "url": search_url, "title": text }));
    }
    // "incog", "private window"...: offer a new incognito window.
    let lowercase = text.to_lowercase();
    let offer_incognito = lowercase.len() >= 3 &&
        (["incognito", "private"].iter().any(|word| word.starts_with(&lowercase)) ||
            (lowercase.len() >= 6 &&
                ["new incognito window", "private window"]
                    .iter()
                    .any(|name| name.contains(&lowercase))));
    if offer_incognito {
        rows.push(json!({
            "kind": "command", "title": "New incognito window", "url": "",
            "command": "incognito-window", "argument": "",
        }));
    }
    let matches = |title: &str, url: &str| {
        let address = url.split_once("://").map_or(url, |(_, rest)| rest);
        words
            .iter()
            .all(|word| contains(title, word) || contains(address, word))
    };
    // Candidates by kind, best first, with how many of each kind may be shown.
    let mut candidates: Vec<(Value, usize)> = Vec::new();
    for pinned in [false, true] {
        for tab in tabs
            .iter()
            .filter(|tab| tab["pinned"].as_bool() == Some(pinned))
        {
            let (title, url) = (
                tab["title"].as_str().unwrap_or_default(),
                tab["url"].as_str().unwrap_or_default(),
            );
            if matches(title, url) {
                let mut row = tab.clone();
                row["kind"] = if pinned { "pin" } else { "tab" }.into();
                candidates.push((row, if pinned { 2 } else { 3 }));
            }
        }
    }
    for bookmark in history.bookmarks.search(&words, 3) {
        candidates.push((
            json!({ "kind": "bookmark", "url": bookmark.url, "title": bookmark.title }),
            3,
        ));
    }
    for (url, title) in history.suggest(&words, MAX_SUGGESTIONS * 2) {
        let starred = history.bookmarks.find(&url).is_some();
        candidates.push((
            json!({ "kind": "history", "url": url, "title": title, "starred": starred }),
            MAX_SUGGESTIONS,
        ));
    }
    // Leave room for a search row at the end when the first row is "Go to".
    let limit = MAX_SUGGESTIONS - usize::from(go_to.is_some());
    let mut seen: Vec<String> = Vec::new();
    let mut per_kind: HashMap<String, usize> = HashMap::new();
    for (row, cap) in candidates {
        let url = row["url"].as_str().unwrap_or_default().to_owned();
        let kind = row["kind"].as_str().unwrap_or_default().to_owned();
        let taken = per_kind.entry(kind).or_default();
        if rows.len() >= limit || *taken >= cap || seen.contains(&url) {
            continue;
        }
        *taken += 1;
        seen.push(url);
        rows.push(row);
    }
    if go_to.is_some() {
        rows.push(json!({ "kind": "search", "url": search_url, "title": text }));
    }
    rows
}

/// Act on a `hist-*` / `bm-*` command from the sidebar page.
pub(crate) fn on_command(
    state: &crate::running_app_state::RunningAppState,
    window: &ServoShellWindow,
    sidebar: &mut Sidebar,
    command: &str,
    argument: &str,
) {
    let mut history = state.history.borrow_mut();
    let (first, rest) = argument.split_once('\t').unwrap_or((argument, ""));
    match command {
        // `<request number>\t<typed text>`
        "hist-suggest" => {
            let started = Instant::now();
            let tabs = sidebar.switch_targets(window);
            let rows = suggestions(&mut history, tabs, rest);
            let elapsed = started.elapsed().as_secs_f64() * 1000.0;
            if elapsed > 10.0 {
                crate::desktop::diagnostics::log(format!(
                    "history: slow suggestions: {elapsed:.1} ms"
                ));
            }
            let reply = json!({ "rows": rows, "ms": (elapsed * 100.0).round() / 100.0 });
            sidebar.call(&format!(
                "histSuggestions({}, {reply})",
                first.parse::<u64>().unwrap_or(0)
            ));
        },
        // The URL bar got focus: load the suggestion index now rather than on the first key.
        "hist-warm" => history.warm(),
        // Optional argument: "history" (the default) or "bookmarks".
        "hist-show" => {
            sidebar.set_focused(true, window);
            let view = if argument == "bookmarks" { "bookmarks" } else { "history" };
            sidebar.call(&format!("openHistory({})", json!(view)));
        },
        // `<before (ms), 0 for the newest>\t<search text>`
        "hist-list" => {
            let before = first
                .parse::<i64>()
                .ok()
                .filter(|before| *before > 0)
                .unwrap_or(i64::MAX);
            let (rows, more) = history.list(before, &words(rest));
            let reply = json!({ "rows": rows, "more": more, "append": before != i64::MAX });
            sidebar.call(&format!("histList({reply})"));
        },
        "hist-delete" => {
            if let Ok(id) = first.parse() {
                history.delete_page(id);
            }
        },
        // `<since (ms), 0 for everything>\t<1 to also clear cookies and site data>`
        "hist-clear" => {
            let cleared = history.clear(first.parse().unwrap_or(0));
            let site_data = rest == "1";
            if site_data {
                clear_site_data(state.servo());
            }
            let message = match (cleared, site_data) {
                (false, _) => "History couldn't be cleared. Try again, or restart Stillwater.",
                (true, true) => "History, cookies and site data cleared.",
                (true, false) => "History cleared.",
            };
            sidebar.call(&format!("histCleared({})", json!(message)));
            sidebar.call_settings(&format!("histCleared({})", json!(message)));
        },
        // Bookmark the page in the URL bar (if it isn't yet) and show its editor.
        "bm-star" => {
            let page = sidebar.navigated_webview(window);
            let Some(url) = page.as_ref().and_then(|page| page.url()) else {
                return;
            };
            if !recordable(url.as_str()) {
                return;
            }
            let bookmark = match history.bookmarks.find(url.as_str()) {
                Some(bookmark) => bookmark.to_json(),
                None => {
                    let title = page
                        .and_then(|page| page.page_title())
                        .filter(|title| !title.trim().is_empty())
                        .unwrap_or_else(|| url.to_string());
                    history.bookmarks.add(url.as_str(), &title, "");
                    history
                        .bookmarks
                        .find(url.as_str())
                        .map(|bookmark| bookmark.to_json())
                        .unwrap_or_default()
                },
            };
            sidebar.set_focused(true, window);
            sidebar.call(&format!("bmEdit({bookmark})"));
        },
        "bm-rename" => {
            if let Ok(id) = first.parse() {
                history.bookmarks.rename(id, rest);
            }
        },
        "bm-delete" => {
            if let Ok(id) = first.parse() {
                history.bookmarks.remove(id);
            }
        },
        "bm-list" => sidebar.call(&format!("bmList({})", history.bookmarks.to_json())),
        _ => warn!("unknown history command: {command}"),
    }
    window.set_needs_update();
}
