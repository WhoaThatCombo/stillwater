/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Import from Chromium-based browsers (Chrome, Edge, Opera, Opera GX, Brave, Vivaldi). They all
//! keep the same files in a "User Data" folder:
//!
//! - `Local State` (JSON) lists the profiles (`profile.info_cache`) and holds the key for
//!   cookies and passwords (`os_crypt.encrypted_key`), protected with Windows DPAPI for the
//!   signed-in user. Decrypting it as that user is the ordinary way to read one's own data.
//! - Each profile folder (`Default`, `Profile 1`, ...; Opera keeps its main profile in the
//!   "User Data" folder itself) has `Bookmarks` (JSON), `History`, `Login Data` and
//!   `Network\Cookies` (SQLite). The browser keeps the databases open, so copies are read.
//!
//! Cookie values and passwords starting with `v10` (or `v11`) are AES-256-GCM with that key.
//! Ones starting with `v20` use Chrome's app-bound encryption, which is made so that only the
//! browser itself can read them: those are counted and reported, never decrypted.
//!
//! Nothing in the browser's folder is changed.

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::desktop::zen_import::{TempCopyDir, ZenCookie, ZenLogin, ZenPlaces, ZenProfile, ZenVisit};

/// Seconds between 1601-01-01 (Chromium's "WebKit" epoch) and 1970-01-01.
const WEBKIT_EPOCH_OFFSET: i64 = 11_644_473_600;

/// Microseconds since 1601 to seconds since 1970 (0 stays 0: "never").
pub(crate) fn webkit_to_unix_seconds(time: i64) -> i64 {
    if time <= 0 {
        0
    } else {
        time / 1_000_000 - WEBKIT_EPOCH_OFFSET
    }
}

/// Whether `path` looks like a Chromium profile folder.
fn is_profile(path: &Path) -> bool {
    ["Preferences", "Bookmarks", "History", "Login Data"]
        .iter()
        .any(|name| path.join(name).is_file())
}

fn local_state(root: &Path) -> Value {
    std::fs::read_to_string(root.join("Local State"))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or(Value::Null)
}

/// The profiles in a "User Data" folder, the last one used first.
pub(crate) fn find_profiles(root: &Path) -> Vec<ZenProfile> {
    let state = local_state(root);
    let last_used = state["profile"]["last_used"].as_str().unwrap_or("Default");
    let info = state["profile"]["info_cache"].as_object();
    let name_of = |dir: &str| {
        info.and_then(|info| info.get(dir))
            .and_then(|profile| profile["name"].as_str())
            .filter(|name| !name.trim().is_empty())
            .map(str::to_owned)
    };
    // (last used, folder name, profile)
    let mut found: Vec<(bool, String, ZenProfile)> = Vec::new();
    // Opera keeps its main profile in the "User Data" folder itself.
    if is_profile(root) {
        found.push((
            true,
            String::new(),
            ZenProfile {
                name: name_of("Default").unwrap_or_else(|| "Main profile".to_owned()),
                path: root.to_owned(),
            },
        ));
    }
    let mut dirs: Vec<String> = info
        .map(|info| info.keys().cloned().collect())
        .unwrap_or_default();
    if dirs.is_empty() {
        dirs = std::fs::read_dir(root)
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .filter_map(|entry| entry.file_name().into_string().ok())
            .filter(|name| name == "Default" || name.starts_with("Profile "))
            .collect();
    }
    for dir in dirs {
        let path = root.join(&dir);
        if !is_profile(&path) || found.iter().any(|(_, _, known)| known.path == path) {
            continue;
        }
        found.push((
            dir == last_used,
            dir.clone(),
            ZenProfile {
                name: name_of(&dir).unwrap_or_else(|| dir.clone()),
                path,
            },
        ));
    }
    // The last used first, then Default, Profile 1, Profile 2...
    let order = |dir: &str| {
        dir.strip_prefix("Profile ")
            .and_then(|number| number.parse::<u32>().ok())
            .map_or(0, |number| number + 1)
    };
    found.sort_by_key(|(last, dir, _)| (!*last, order(dir), dir.clone()));
    found.into_iter().map(|(_, _, profile)| profile).collect()
}

// ---- Decryption ----

/// What a stored cookie value or password turned out to be.
enum Secret {
    Plain(Vec<u8>),
    /// App-bound (`v20`): only the browser itself can read it.
    Protected,
    Unreadable,
}

/// The AES key from `Local State`, if there is one and DPAPI unlocks it for this user.
fn master_key(root: &Path) -> Option<Vec<u8>> {
    use base64::Engine;
    let state = local_state(root);
    let encoded = state["os_crypt"]["encrypted_key"].as_str()?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .ok()?;
    let blob = bytes.strip_prefix(b"DPAPI")?;
    let key = crate::desktop::passwords::unprotect_for_user(blob)?;
    (key.len() == 32).then_some(key)
}

fn decrypt(value: &[u8], key: Option<&[u8]>) -> Secret {
    use aes_gcm::aead::Aead;
    use aes_gcm::{Aes256Gcm, KeyInit};
    if value.starts_with(b"v20") {
        return Secret::Protected;
    }
    if value.starts_with(b"v10") || value.starts_with(b"v11") {
        let (Some(key), Some(nonce), Some(ciphertext)) = (key, value.get(3..15), value.get(15..))
        else {
            return Secret::Unreadable;
        };
        let Ok(cipher) = Aes256Gcm::new_from_slice(key) else {
            return Secret::Unreadable;
        };
        let Ok(nonce) = aes_gcm::Nonce::<aes_gcm::aead::consts::U12>::try_from(nonce) else {
            return Secret::Unreadable;
        };
        return match cipher.decrypt(&nonce, ciphertext) {
            Ok(plain) => Secret::Plain(plain),
            Err(_) => Secret::Unreadable,
        };
    }
    // Before Chrome 80 (2020) each value was a DPAPI blob of its own.
    if value.starts_with(&[1, 0, 0, 0]) {
        return crate::desktop::passwords::unprotect_for_user(value)
            .map_or(Secret::Unreadable, Secret::Plain);
    }
    Secret::Unreadable
}

/// Open a copy of one of the profile's databases (with its journal or write-ahead log).
fn open_copy(
    source: &Path,
    temp: &TempCopyDir,
    name: &str,
) -> Result<rusqlite::Connection, ()> {
    let copy = temp.0.join(name);
    std::fs::copy(source, &copy).map_err(|_| ())?;
    for suffix in ["-wal", "-journal"] {
        let mut extra = source.as_os_str().to_owned();
        extra.push(suffix);
        let extra = PathBuf::from(extra);
        if extra.exists() {
            let mut target = copy.as_os_str().to_owned();
            target.push(suffix);
            let _ = std::fs::copy(&extra, PathBuf::from(target));
        }
    }
    rusqlite::Connection::open(&copy).map_err(|_| ())
}

fn has_column(connection: &rusqlite::Connection, table: &str, column: &str) -> bool {
    connection
        .prepare(&format!("SELECT 1 FROM pragma_table_info('{table}') WHERE name = ?1"))
        .and_then(|mut statement| statement.exists([column]))
        .unwrap_or(false)
}

/// `column` if the table has it, else `fallback` (older browser versions lack some).
fn column_or(connection: &rusqlite::Connection, table: &str, column: &str, fallback: &str) -> String {
    if has_column(connection, table, column) {
        column.to_owned()
    } else {
        fallback.to_owned()
    }
}

// ---- Everything from one profile ----

pub(crate) struct ChromiumData {
    pub(crate) places: Result<ZenPlaces, String>,
    pub(crate) logins: Result<Vec<ZenLogin>, String>,
    pub(crate) cookies: Result<Vec<ZenCookie>, String>,
    /// Logins for apps or HTTP authentication, or that couldn't be decrypted.
    pub(crate) skipped_logins: usize,
    /// Passwords and cookies under app-bound encryption.
    pub(crate) protected_logins: usize,
    pub(crate) protected_cookies: usize,
    /// Cookies that couldn't be decrypted.
    pub(crate) unreadable_cookies: usize,
    /// Cookies that end when the browser closes.
    pub(crate) session_cookies: usize,
}

/// `root` is the "User Data" folder, `profile` one of its profiles, `browser` the name to report.
pub(crate) fn read_profile(root: &Path, profile: &Path, browser: &str) -> ChromiumData {
    let key = master_key(root);
    let key = key.as_deref();
    let places = read_places(profile, browser);
    let (logins, skipped_logins, protected_logins) = match read_logins(profile, key, browser) {
        Ok((logins, skipped, protected)) => (Ok(logins), skipped, protected),
        Err(error) => (Err(error), 0, 0),
    };
    let mut counts = CookieCounts::default();
    let cookies = read_cookies(profile, key, browser, &mut counts);
    ChromiumData {
        places,
        logins,
        cookies,
        skipped_logins,
        protected_logins,
        protected_cookies: counts.protected,
        unreadable_cookies: counts.unreadable,
        session_cookies: counts.session,
    }
}

fn copy_problem(what: &str, browser: &str) -> String {
    format!("{browser}'s {what} couldn't be read. Close {browser} and try again.")
}

// ---- Bookmarks ----

fn read_places(profile: &Path, browser: &str) -> Result<ZenPlaces, String> {
    let bookmarks = match std::fs::read_to_string(profile.join("Bookmarks")) {
        Ok(text) => {
            let value: Value = serde_json::from_str(&text)
                .map_err(|_| format!("{browser}'s bookmarks file couldn't be read."))?;
            let mut list = Vec::new();
            for (key, root) in value["roots"].as_object().into_iter().flatten() {
                // Opera keeps deleted bookmarks in a "trash" folder.
                if key == "trash" || !root.is_object() {
                    continue;
                }
                let name = match key.as_str() {
                    "bookmark_bar" => "Bookmarks bar".to_owned(),
                    "other" => "Other bookmarks".to_owned(),
                    "synced" => "Mobile bookmarks".to_owned(),
                    _ => root["name"].as_str().unwrap_or("Bookmarks").to_owned(),
                };
                collect_bookmarks(root, &name, &mut list, 0);
            }
            list.sort_by_key(|(_, _, _, added)| *added);
            list
        },
        Err(_) => Vec::new(),
    };
    let visits = read_history(profile, browser)?;
    Ok(ZenPlaces { bookmarks, visits })
}

fn collect_bookmarks(
    node: &Value,
    folder: &str,
    list: &mut Vec<(String, String, String, u64)>,
    depth: usize,
) {
    if depth > 64 {
        return;
    }
    for child in node["children"].as_array().into_iter().flatten() {
        match child["type"].as_str() {
            Some("url") => {
                let Some(url) = child["url"].as_str() else {
                    continue;
                };
                if !(url.starts_with("http://") || url.starts_with("https://")) {
                    continue;
                }
                let added = child["date_added"]
                    .as_str()
                    .and_then(|time| time.parse::<i64>().ok())
                    .map_or(0, webkit_to_unix_seconds)
                    .max(0) as u64;
                list.push((
                    url.to_owned(),
                    child["name"].as_str().unwrap_or_default().to_owned(),
                    folder.to_owned(),
                    added,
                ));
            },
            Some("folder") => {
                let name = child["name"].as_str().unwrap_or(folder);
                collect_bookmarks(child, name, list, depth + 1);
            },
            _ => {},
        }
    }
}

// ---- History ----

fn read_history(profile: &Path, browser: &str) -> Result<Vec<ZenVisit>, String> {
    let source = profile.join("History");
    if !source.exists() {
        return Ok(Vec::new());
    }
    let temp = TempCopyDir::new()?;
    let connection =
        open_copy(&source, &temp, "History").map_err(|_| copy_problem("history", browser))?;
    // Links, typed addresses, bookmarks, generated (searches), form submissions and keywords;
    // not frames, reloads or start pages. The core type is the low byte of `transition`.
    let cutoff_seconds = crate::desktop::history::now_ms() / 1000 -
        crate::desktop::history::KEEP_DAYS * 24 * 60 * 60;
    let cutoff = (cutoff_seconds + WEBKIT_EPOCH_OFFSET) * 1_000_000;
    let hidden = column_or(&connection, "urls", "hidden", "0");
    let query = format!(
        "SELECT u.url, coalesce(u.title, ''), v.visit_time
         FROM visits v JOIN urls u ON u.id = v.url
         WHERE {hidden} = 0 AND (v.transition & 255) IN (0, 1, 2, 5, 7, 9) AND v.visit_time >= ?1
         ORDER BY v.visit_time"
    );
    let result = (|| -> rusqlite::Result<Vec<ZenVisit>> {
        let mut statement = connection.prepare(&query)?;
        let visits = statement
            .query_map([cutoff], |row| {
                Ok(ZenVisit {
                    url: row.get(0)?,
                    title: row.get(1)?,
                    // Milliseconds since 1970.
                    time: row.get::<_, i64>(2)? / 1000 - WEBKIT_EPOCH_OFFSET * 1000,
                })
            })?
            .filter_map(Result::ok)
            .filter(|visit| crate::desktop::history::recordable(&visit.url))
            .collect();
        Ok(visits)
    })();
    drop(connection);
    drop(temp);
    result.map_err(|_| format!("{browser}'s history couldn't be read."))
}

// ---- Passwords ----

/// Saved logins from `Login Data` (and `Login Data For Account`, passwords Chrome keeps for a
/// Google account): (logins, skipped, protected).
fn read_logins(
    profile: &Path,
    key: Option<&[u8]>,
    browser: &str,
) -> Result<(Vec<ZenLogin>, usize, usize), String> {
    let mut logins: Vec<ZenLogin> = Vec::new();
    let (mut skipped, mut protected, mut unreadable) = (0, 0, 0);
    for file in ["Login Data", "Login Data For Account"] {
        let source = profile.join(file);
        if !source.exists() {
            continue;
        }
        let temp = TempCopyDir::new()?;
        let connection = open_copy(&source, &temp, "logins")
            .map_err(|_| copy_problem("saved passwords", browser))?;
        let last_used = column_or(&connection, "logins", "date_last_used", "0");
        let scheme = column_or(&connection, "logins", "scheme", "0");
        let query = format!(
            "SELECT origin_url, username_value, password_value, date_created, {last_used},
                    times_used, blacklisted_by_user, {scheme}
             FROM logins"
        );
        type Row = (String, String, Vec<u8>, i64, i64, i64, i64, i64);
        let rows: rusqlite::Result<Vec<Row>> = (|| {
            let mut statement = connection.prepare(&query)?;
            let rows = statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?.unwrap_or_default(),
                        row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                        row.get::<_, Option<Vec<u8>>>(2)?.unwrap_or_default(),
                        row.get::<_, Option<i64>>(3)?.unwrap_or(0),
                        row.get::<_, Option<i64>>(4)?.unwrap_or(0),
                        row.get::<_, Option<i64>>(5)?.unwrap_or(0),
                        row.get::<_, Option<i64>>(6)?.unwrap_or(0),
                        row.get::<_, Option<i64>>(7)?.unwrap_or(0),
                    ))
                })?
                .filter_map(Result::ok)
                .collect();
            Ok(rows)
        })();
        let rows = rows.map_err(|_| format!("{browser}'s saved passwords couldn't be read."))?;
        drop(connection);
        drop(temp);
        for (origin_url, username, password, created, used, times, never, scheme) in rows {
            // "Never save for this site" entries aren't logins.
            if never != 0 {
                continue;
            }
            // Apps (android://), HTTP authentication (scheme 1, 2) and other odd entries.
            let origin = url::Url::parse(&origin_url)
                .ok()
                .filter(|url| matches!(url.scheme(), "http" | "https"))
                .map(|url| url.origin().ascii_serialization());
            let Some(origin) = origin.filter(|_| scheme == 0) else {
                skipped += 1;
                continue;
            };
            let password = match decrypt(&password, key) {
                Secret::Plain(bytes) => String::from_utf8(bytes).ok(),
                Secret::Protected => {
                    protected += 1;
                    continue;
                },
                Secret::Unreadable => None,
            };
            let Some(password) = password.filter(|password| !password.is_empty()) else {
                unreadable += 1;
                continue;
            };
            if logins
                .iter()
                .any(|known| known.origin == origin && known.username == username)
            {
                continue;
            }
            logins.push(ZenLogin {
                origin,
                username,
                password,
                created: webkit_to_unix_seconds(created).max(0) as u64,
                last_used: webkit_to_unix_seconds(used).max(0) as u64,
                times_used: times.max(0) as u64,
            });
        }
    }
    // Without the key nothing encrypted can be read: say why, rather than "skipped".
    if unreadable > 0 && logins.is_empty() && key.is_none() {
        return Err(format!(
            "{browser}'s passwords couldn't be decrypted with this Windows account."
        ));
    }
    Ok((logins, skipped + unreadable, protected))
}

// ---- Cookies ----

#[derive(Default)]
struct CookieCounts {
    protected: usize,
    unreadable: usize,
    session: usize,
}

fn read_cookies(
    profile: &Path,
    key: Option<&[u8]>,
    browser: &str,
    counts: &mut CookieCounts,
) -> Result<Vec<ZenCookie>, String> {
    use sha2::{Digest, Sha256};
    // Newer versions keep cookies in Network\Cookies, older ones next to the other files.
    let Some(source) = [profile.join("Network").join("Cookies"), profile.join("Cookies")]
        .into_iter()
        .find(|path| path.exists())
    else {
        return Ok(Vec::new());
    };
    let temp = TempCopyDir::new()?;
    let connection =
        open_copy(&source, &temp, "Cookies").map_err(|_| copy_problem("cookies", browser))?;
    // From version 24 the decrypted value starts with a SHA-256 of the cookie's host.
    let version: i64 = connection
        .query_row("SELECT value FROM meta WHERE key = 'version'", [], |row| {
            row.get::<_, String>(0)
        })
        .ok()
        .and_then(|version| version.parse().ok())
        .unwrap_or(0);
    let same_site = column_or(&connection, "cookies", "samesite", "-1");
    let persistent = column_or(&connection, "cookies", "is_persistent", "1");
    let partition = column_or(&connection, "cookies", "top_frame_site_key", "''");
    let query = format!(
        "SELECT host_key, name, value, encrypted_value, path, expires_utc, is_secure, is_httponly,
                {same_site}, {persistent}, coalesce({partition}, '')
         FROM cookies ORDER BY host_key, path, name"
    );
    type Row = (String, String, String, Vec<u8>, String, i64, i64, i64, i64, i64, String);
    let rows: rusqlite::Result<Vec<Row>> = (|| {
        let mut statement = connection.prepare(&query)?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?.unwrap_or_default(),
                    row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                    row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                    row.get::<_, Option<Vec<u8>>>(3)?.unwrap_or_default(),
                    row.get::<_, Option<String>>(4)?.unwrap_or_default(),
                    row.get::<_, Option<i64>>(5)?.unwrap_or(0),
                    row.get::<_, Option<i64>>(6)?.unwrap_or(0),
                    row.get::<_, Option<i64>>(7)?.unwrap_or(0),
                    row.get::<_, Option<i64>>(8)?.unwrap_or(-1),
                    row.get::<_, Option<i64>>(9)?.unwrap_or(1),
                    row.get::<_, String>(10)?,
                ))
            })?
            .filter_map(Result::ok)
            .collect();
        Ok(rows)
    })();
    drop(connection);
    drop(temp);
    let rows = rows.map_err(|_| format!("{browser}'s cookies couldn't be read."))?;
    let now = crate::desktop::history::now_ms() / 1000;
    let mut cookies = Vec::new();
    for (host, name, value, encrypted, path, expires, secure, http_only, same_site, persistent, partition) in rows {
        // Partitioned (third-party, CHIPS) cookies, as with Zen's.
        if !partition.is_empty() {
            continue;
        }
        // Session cookies end when the browser closes; Zen's file never has them either.
        if persistent == 0 {
            counts.session += 1;
            continue;
        }
        let value = if encrypted.is_empty() {
            value
        } else {
            match decrypt(&encrypted, key) {
                Secret::Plain(mut bytes) => {
                    if version >= 24 &&
                        bytes.len() >= 32 &&
                        bytes[..32] == Sha256::digest(host.as_bytes())[..]
                    {
                        bytes.drain(..32);
                    }
                    match String::from_utf8(bytes) {
                        Ok(value) => value,
                        Err(_) => {
                            counts.unreadable += 1;
                            continue;
                        },
                    }
                },
                Secret::Protected => {
                    counts.protected += 1;
                    continue;
                },
                Secret::Unreadable => {
                    counts.unreadable += 1;
                    continue;
                },
            }
        };
        let cookie = ZenCookie {
            name,
            value,
            host,
            path,
            expiry: webkit_to_unix_seconds(expires),
            secure: secure != 0,
            http_only: http_only != 0,
            // Chromium: -1 unspecified, 0 none, 1 lax, 2 strict; Firefox's numbers otherwise.
            same_site,
        };
        if crate::desktop::zen_import::usable_cookie(&cookie, now) {
            cookies.push(cookie);
        }
    }
    Ok(cookies)
}
