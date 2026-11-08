/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Import from Zen Browser: spaces, pinned tabs, Essentials, folders, open tabs and saved
//! passwords. Zen is Firefox-based, so this reads Firefox's profile files:
//!
//! - `profiles.ini` / `installs.ini` list the profiles.
//! - The session (`sessionstore-backups/recovery.jsonlz4` while Zen runs, `sessionstore.jsonlz4`
//!   after it closes) holds Zen's spaces, folders and tabs. `.jsonlz4` is Mozilla's LZ4 format.
//! - `logins.json` holds the passwords, encrypted with a key kept in `key4.db` (NSS). Without a
//!   primary password the key is protected by an empty password, so it can be read.
//!
//! - `places.sqlite` holds bookmarks (`moz_bookmarks`) and history (`moz_places`,
//!   `moz_historyvisits`). Zen keeps it open, so a copy is read.
//!
//! Nothing in the Zen profile is changed.

use std::path::{Path, PathBuf};

use aes::Aes256;
use cbc::Decryptor;
use cbc::cipher::block_padding::Pkcs7;
use cbc::cipher::{BlockModeDecrypt, KeyIvInit};
use serde_json::Value;
use sha1::{Digest, Sha1};
use sha2::Sha256;

pub(crate) struct ZenProfile {
    pub(crate) name: String,
    pub(crate) path: PathBuf,
}

pub(crate) struct ZenSpace {
    pub(crate) uuid: String,
    pub(crate) name: String,
    /// The space's main gradient colour, if it has one.
    pub(crate) color: Option<[u8; 3]>,
}

pub(crate) struct ZenFolder {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) space: Option<String>,
    pub(crate) collapsed: bool,
}

pub(crate) struct ZenTab {
    pub(crate) url: String,
    pub(crate) title: String,
    pub(crate) space: Option<String>,
    pub(crate) pinned: bool,
    pub(crate) essential: bool,
    pub(crate) folder: Option<String>,
}

#[derive(Default)]
pub(crate) struct ZenSession {
    pub(crate) spaces: Vec<ZenSpace>,
    pub(crate) folders: Vec<ZenFolder>,
    pub(crate) tabs: Vec<ZenTab>,
}

pub(crate) struct ZenLogin {
    pub(crate) origin: String,
    pub(crate) username: String,
    pub(crate) password: String,
    /// Seconds since 1970.
    pub(crate) created: u64,
    pub(crate) last_used: u64,
    pub(crate) times_used: u64,
}

/// A visit to a page, from Zen's history.
pub(crate) struct ZenVisit {
    pub(crate) url: String,
    pub(crate) title: String,
    /// Milliseconds since 1970.
    pub(crate) time: i64,
}

/// Zen's bookmarks and recent history.
#[derive(Default)]
pub(crate) struct ZenPlaces {
    /// (url, title, folder name, added in seconds since 1970), oldest first.
    pub(crate) bookmarks: Vec<(String, String, String, u64)>,
    /// Visits from the last 90 days (what Stillwater keeps), oldest first.
    pub(crate) visits: Vec<ZenVisit>,
}

/// Everything read from one Zen profile.
pub(crate) struct ZenData {
    pub(crate) session: Result<ZenSession, String>,
    pub(crate) places: Result<ZenPlaces, String>,
    pub(crate) logins: Result<Vec<ZenLogin>, String>,
    /// Saved logins that can't be imported (HTTP authentication, unreadable).
    pub(crate) skipped_logins: usize,
}

/// Zen's data folder (`STILLWATER_ZEN_DIR` overrides it for tests).
fn zen_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("STILLWATER_ZEN_DIR") {
        return Some(PathBuf::from(dir));
    }
    if cfg!(any(windows, target_os = "macos")) {
        dirs::config_dir().map(|dir| dir.join("zen"))
    } else {
        dirs::home_dir().map(|dir| dir.join(".zen"))
    }
}

/// Sections of an INI file: (section name, key/value pairs).
fn parse_ini(text: &str) -> Vec<(String, Vec<(String, String)>)> {
    let mut sections: Vec<(String, Vec<(String, String)>)> = Vec::new();
    for line in text.lines().map(str::trim) {
        if let Some(name) = line
            .strip_prefix('[')
            .and_then(|line| line.strip_suffix(']'))
        {
            sections.push((name.to_owned(), Vec::new()));
        } else if let (Some((key, value)), Some((_, entries))) =
            (line.split_once('='), sections.last_mut())
        {
            entries.push((key.trim().to_owned(), value.trim().to_owned()));
        }
    }
    sections
}

fn ini_value<'a>(entries: &'a [(String, String)], key: &str) -> Option<&'a str> {
    entries
        .iter()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value.as_str())
}

/// Zen's profiles, the one Zen opens by default first.
pub(crate) fn find_profiles() -> Vec<ZenProfile> {
    let Some(root) = zen_dir() else {
        return Vec::new();
    };
    let read = |name: &str| std::fs::read_to_string(root.join(name)).unwrap_or_default();
    let install_defaults: Vec<String> = parse_ini(&read("installs.ini"))
        .iter()
        .filter_map(|(_, entries)| ini_value(entries, "Default").map(str::to_owned))
        .collect();
    let mut profiles: Vec<(bool, ZenProfile)> = Vec::new();
    for (section, entries) in parse_ini(&read("profiles.ini")) {
        if !section.starts_with("Profile") {
            continue;
        }
        let Some(path) = ini_value(&entries, "Path") else {
            continue;
        };
        let full = if ini_value(&entries, "IsRelative") == Some("0") {
            PathBuf::from(path)
        } else {
            root.join(path)
        };
        if !full.join("prefs.js").exists() && !full.join("logins.json").exists() {
            continue;
        }
        let preferred = install_defaults.iter().any(|default| default == path) ||
            ini_value(&entries, "Default") == Some("1");
        profiles.push((
            preferred,
            ZenProfile {
                name: ini_value(&entries, "Name").unwrap_or(path).to_owned(),
                path: full,
            },
        ));
    }
    // Stable sort: preferred profiles first, otherwise in profiles.ini order.
    profiles.sort_by_key(|(preferred, _)| !*preferred);
    profiles.into_iter().map(|(_, profile)| profile).collect()
}

pub(crate) fn read_profile(profile: &Path) -> ZenData {
    let (logins, skipped_logins) = match read_logins(profile) {
        Ok((logins, skipped)) => (Ok(logins), skipped),
        Err(error) => (Err(error), 0),
    };
    ZenData {
        session: read_session(profile),
        places: read_places(profile),
        logins,
        skipped_logins,
    }
}

// ---- Session: spaces, folders and tabs ----

/// Decompress Mozilla's `.jsonlz4` / `.mozlz4` files: a magic number, the decompressed size,
/// then one LZ4 block.
fn read_mozlz4(path: &Path) -> Option<Vec<u8>> {
    let data = std::fs::read(path).ok()?;
    let body = data.strip_prefix(b"mozLz40\0")?;
    let size = u32::from_le_bytes(body.get(..4)?.try_into().ok()?) as usize;
    lz4_block_decompress(&body[4..], size)
}

fn lz4_block_decompress(source: &[u8], size: usize) -> Option<Vec<u8>> {
    // A session is a few MB; refuse absurd sizes from a damaged file.
    if size > 512 * 1024 * 1024 {
        return None;
    }
    let mut out: Vec<u8> = Vec::with_capacity(size);
    let mut i = 0;
    let read_length = |i: &mut usize, mut length: usize| -> Option<usize> {
        if length == 15 {
            loop {
                let byte = *source.get(*i)?;
                *i += 1;
                length += byte as usize;
                if byte != 255 {
                    break;
                }
            }
        }
        Some(length)
    };
    while i < source.len() {
        let token = source[i];
        i += 1;
        let literals = read_length(&mut i, (token >> 4) as usize)?;
        out.extend_from_slice(source.get(i..i + literals)?);
        i += literals;
        if i >= source.len() {
            break;
        }
        let offset = u16::from_le_bytes([source[i], *source.get(i + 1)?]) as usize;
        i += 2;
        let length = read_length(&mut i, (token & 15) as usize)? + 4;
        if offset == 0 || offset > out.len() || out.len() + length > size {
            return None;
        }
        let start = out.len() - offset;
        for k in 0..length {
            out.push(out[start + k]);
        }
    }
    (out.len() == size).then_some(out)
}

/// The newest session file: Zen keeps the live session in `recovery.jsonlz4` while it runs
/// and writes `sessionstore.jsonlz4` when it closes.
fn session_file(profile: &Path) -> Option<PathBuf> {
    let candidates = [
        profile.join("sessionstore.jsonlz4"),
        profile
            .join("sessionstore-backups")
            .join("recovery.jsonlz4"),
        profile
            .join("sessionstore-backups")
            .join("previous.jsonlz4"),
    ];
    candidates
        .into_iter()
        .filter_map(|path| {
            let modified = path.metadata().ok()?.modified().ok()?;
            Some((modified, path))
        })
        .max_by_key(|(modified, _)| *modified)
        .map(|(_, path)| path)
}

fn read_session(profile: &Path) -> Result<ZenSession, String> {
    let path = session_file(profile).ok_or("Zen hasn't saved a session in this profile yet.")?;
    let bytes = read_mozlz4(&path).ok_or("Zen's session file couldn't be read.")?;
    let value: Value =
        serde_json::from_slice(&bytes).map_err(|_| "Zen's session file couldn't be read.")?;
    let mut session = ZenSession::default();
    for window in value["windows"].as_array().into_iter().flatten() {
        for space in window["spaces"].as_array().into_iter().flatten() {
            let Some(uuid) = space["uuid"].as_str() else {
                continue;
            };
            if session.spaces.iter().any(|known| known.uuid == uuid) {
                continue;
            }
            session.spaces.push(ZenSpace {
                uuid: uuid.to_owned(),
                name: space["name"].as_str().unwrap_or("Space").to_owned(),
                color: space_color(&space["theme"]),
            });
        }
        for folder in window["folders"].as_array().into_iter().flatten() {
            let Some(id) = folder["id"].as_str() else {
                continue;
            };
            if session.folders.iter().any(|known| known.id == id) {
                continue;
            }
            session.folders.push(ZenFolder {
                id: id.to_owned(),
                name: folder["name"].as_str().unwrap_or("Folder").to_owned(),
                space: folder["workspaceId"].as_str().map(str::to_owned),
                collapsed: folder["collapsed"].as_bool().unwrap_or(false),
            });
        }
        for tab in window["tabs"].as_array().into_iter().flatten() {
            if let Some(tab) = session_tab(tab) {
                session.tabs.push(tab);
            }
        }
    }
    Ok(session)
}

/// A space's colour: the primary (or first) colour of its gradient.
fn space_color(theme: &Value) -> Option<[u8; 3]> {
    let colors = theme["gradientColors"].as_array()?;
    let color = colors
        .iter()
        .find(|color| color["isPrimary"].as_bool() == Some(true))
        .or_else(|| colors.first())?;
    let channels = color["c"].as_array()?;
    let channel = |index: usize| {
        channels
            .get(index)?
            .as_f64()
            .map(|value| value.clamp(0.0, 255.0) as u8)
    };
    Some([channel(0)?, channel(1)?, channel(2)?])
}

fn session_tab(tab: &Value) -> Option<ZenTab> {
    // Folder placeholders and Glance previews aren't real tabs.
    if tab["zenIsEmpty"].as_bool() == Some(true) || tab["zenIsGlance"].as_bool() == Some(true) {
        return None;
    }
    let pinned = tab["pinned"].as_bool() == Some(true);
    let essential = tab["zenEssential"].as_bool() == Some(true);
    // A pinned tab goes back to its pinned page; otherwise use the page the tab shows.
    let entries = tab["entries"].as_array();
    let current = entries.and_then(|entries| {
        let index = tab["index"].as_u64().unwrap_or(entries.len() as u64) as usize;
        entries
            .get(index.saturating_sub(1))
            .or_else(|| entries.last())
    });
    let initial = &tab["_zenPinnedInitialState"]["entry"];
    let entry = if (pinned || essential) && initial["url"].is_string() {
        initial
    } else {
        current?
    };
    let url = entry["url"].as_str()?;
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return None;
    }
    Some(ZenTab {
        url: url.to_owned(),
        title: entry["title"].as_str().unwrap_or_default().to_owned(),
        space: tab["zenWorkspace"].as_str().map(str::to_owned),
        pinned,
        essential,
        folder: tab["groupId"].as_str().map(str::to_owned),
    })
}

// ---- Passwords: logins.json + key4.db ----

/// A DER element: (tag, contents, rest of the input).
fn der(input: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let tag = *input.first()?;
    let first = *input.get(1)? as usize;
    let (length, start) = if first < 0x80 {
        (first, 2)
    } else {
        let count = first & 0x7f;
        if count == 0 || count > 4 {
            return None;
        }
        let length = input
            .get(2..2 + count)?
            .iter()
            .fold(0usize, |length, byte| length << 8 | *byte as usize);
        (length, 2 + count)
    };
    let end = start.checked_add(length)?;
    Some((tag, input.get(start..end)?, &input[end..]))
}

/// The elements inside a DER SEQUENCE's contents.
fn der_items(mut input: &[u8]) -> Option<Vec<(u8, &[u8])>> {
    let mut items = Vec::new();
    while !input.is_empty() {
        let (tag, contents, rest) = der(input)?;
        items.push((tag, contents));
        input = rest;
    }
    Some(items)
}

fn der_integer(bytes: &[u8]) -> Option<u64> {
    (bytes.len() <= 8).then(|| {
        bytes
            .iter()
            .fold(0u64, |value, byte| value << 8 | *byte as u64)
    })
}

// Object identifiers (DER contents).
const OID_PBES2: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x05, 0x0d];
const OID_AES256_CBC: &[u8] = &[0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x01, 0x2a];

fn aes256_cbc_decrypt(key: &[u8], iv: &[u8], ciphertext: &[u8]) -> Option<Vec<u8>> {
    let decryptor = Decryptor::<Aes256>::new_from_slices(key.get(..32)?, iv).ok()?;
    decryptor.decrypt_padded_vec::<Pkcs7>(ciphertext).ok()
}

/// Decrypt an NSS key-store item (PBES2: PBKDF2-HMAC-SHA256 + AES-256-CBC) protected by an
/// empty primary password.
fn decrypt_pbe(item: &[u8], global_salt: &[u8]) -> Option<Vec<u8>> {
    let (_, top, _) = der(item)?;
    let top = der_items(top)?;
    let algorithm = der_items(top.first()?.1)?;
    if algorithm.first()?.1 != OID_PBES2 {
        // Profiles from before 2019 use 3DES; Zen has never written those.
        return None;
    }
    let parameters = der_items(algorithm.get(1)?.1)?;
    let kdf = der_items(parameters.first()?.1)?;
    let kdf_parameters = der_items(kdf.get(1)?.1)?;
    let salt = kdf_parameters.first()?.1;
    let iterations = der_integer(kdf_parameters.get(1)?.1)?;
    if iterations == 0 || iterations > 10_000_000 {
        return None;
    }
    let cipher = der_items(parameters.get(1)?.1)?;
    if cipher.first()?.1 != OID_AES256_CBC {
        return None;
    }
    // NSS stores the IV without its first two bytes (an OCTET STRING header, 04 0e).
    let mut iv = cipher.get(1)?.1.to_vec();
    if iv.len() == 14 {
        iv.splice(0..0, [0x04, 0x0e]);
    }
    let password_key = Sha1::digest(global_salt);
    let mut key = [0u8; 32];
    pbkdf2::pbkdf2_hmac::<Sha256>(&password_key, salt, iterations as u32, &mut key);
    aes256_cbc_decrypt(&key, &iv, top.get(1)?.1)
}

/// The keys in `key4.db`, by key id.
fn read_keys(profile: &Path) -> Result<Vec<(Vec<u8>, Vec<u8>)>, String> {
    // Read a copy: Zen may have the database open.
    let copy = std::env::temp_dir().join(format!("stillwater-zen-key4-{}.db", std::process::id()));
    std::fs::copy(profile.join("key4.db"), &copy)
        .map_err(|_| "Zen's key store (key4.db) couldn't be read.")?;
    let result = (|| {
        let connection = rusqlite::Connection::open_with_flags(
            &copy,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|_| "Zen's key store couldn't be opened.")?;
        let (salt, check): (Vec<u8>, Vec<u8>) = connection
            .query_row(
                "SELECT item1, item2 FROM metaData WHERE id = 'password'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(|_| "Zen's key store is empty.")?;
        match decrypt_pbe(&check, &salt) {
            Some(text) if text.starts_with(b"password-check") => {},
            _ => {
                return Err(
                    "Zen's passwords are protected by a primary password. Remove it in Zen's settings (Passwords), import, then set it again.",
                );
            },
        }
        let mut statement = connection
            .prepare("SELECT a11, a102 FROM nssPrivate")
            .map_err(|_| "Zen's key store has no keys.")?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?))
            })
            .map_err(|_| "Zen's key store has no keys.")?;
        Ok(rows
            .filter_map(Result::ok)
            .filter_map(|(item, id)| Some((id, decrypt_pbe(&item, &salt)?)))
            .collect())
    })();
    let _ = std::fs::remove_file(&copy);
    result.map_err(str::to_owned)
}

/// Decrypt one `encryptedUsername` / `encryptedPassword` value.
fn decrypt_field(value: &str, keys: &[(Vec<u8>, Vec<u8>)]) -> Option<String> {
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(value)
        .ok()?;
    let (_, top, _) = der(&bytes)?;
    let top = der_items(top)?;
    let key_id = top.first()?.1;
    let algorithm = der_items(top.get(1)?.1)?;
    if algorithm.first()?.1 != OID_AES256_CBC {
        return None;
    }
    let iv = algorithm.get(1)?.1;
    let (_, key) = keys.iter().find(|(id, _)| id == key_id)?;
    String::from_utf8(aes256_cbc_decrypt(key, iv, top.get(2)?.1)?).ok()
}

/// Zen's saved logins and how many were skipped.
fn read_logins(profile: &Path) -> Result<(Vec<ZenLogin>, usize), String> {
    let Ok(text) = std::fs::read_to_string(profile.join("logins.json")) else {
        return Ok((Vec::new(), 0));
    };
    let value: Value =
        serde_json::from_str(&text).map_err(|_| "Zen's logins.json couldn't be read.")?;
    let entries: Vec<&Value> = value["logins"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|login| login["deleted"].as_bool() != Some(true))
        .collect();
    if entries.is_empty() {
        return Ok((Vec::new(), 0));
    }
    let keys = read_keys(profile)?;
    let seconds = |login: &Value, key: &str| login[key].as_u64().unwrap_or(0) / 1000;
    let mut logins = Vec::new();
    let mut skipped = 0;
    for login in entries {
        // Logins for HTTP authentication prompts, not web pages.
        let origin = login["hostname"].as_str().unwrap_or_default();
        if !login["httpRealm"].is_null() ||
            !(origin.starts_with("http://") || origin.starts_with("https://"))
        {
            skipped += 1;
            continue;
        }
        let (Some(username), Some(password)) = (
            login["encryptedUsername"]
                .as_str()
                .and_then(|value| decrypt_field(value, &keys)),
            login["encryptedPassword"]
                .as_str()
                .and_then(|value| decrypt_field(value, &keys)),
        ) else {
            skipped += 1;
            continue;
        };
        logins.push(ZenLogin {
            origin: origin.to_owned(),
            username,
            password,
            created: seconds(login, "timeCreated"),
            last_used: seconds(login, "timeLastUsed"),
            times_used: login["timesUsed"].as_u64().unwrap_or(0),
        });
    }
    Ok((logins, skipped))
}

// ---- Bookmarks and history: places.sqlite ----

fn read_places(profile: &Path) -> Result<ZenPlaces, String> {
    let source = profile.join("places.sqlite");
    if !source.exists() {
        return Ok(ZenPlaces::default());
    }
    // Copy the database and its write-ahead log (recent changes while Zen runs) and read the
    // copy: Zen keeps the original open and locked.
    let copy = std::env::temp_dir().join(format!("stillwater-zen-places-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&copy);
    let result = (|| {
        std::fs::create_dir_all(&copy).map_err(|_| "Zen's history couldn't be copied.")?;
        std::fs::copy(&source, copy.join("places.sqlite"))
            .map_err(|_| "Zen's bookmarks and history (places.sqlite) couldn't be read.")?;
        let wal = profile.join("places.sqlite-wal");
        if wal.exists() {
            let _ = std::fs::copy(&wal, copy.join("places.sqlite-wal"));
        }
        let connection = rusqlite::Connection::open(copy.join("places.sqlite"))
            .map_err(|_| "Zen's bookmarks and history couldn't be opened.")?;
        read_places_from(&connection).map_err(|_| "Zen's bookmarks and history couldn't be read.")
    })();
    let _ = std::fs::remove_dir_all(&copy);
    result.map_err(str::to_owned)
}

fn read_places_from(connection: &rusqlite::Connection) -> rusqlite::Result<ZenPlaces> {
    // Bookmarks (type 1), except tags (bookmarks inside the tags root's folders).
    let mut statement = connection.prepare(
        "SELECT p.url, coalesce(b.title, p.title, ''), coalesce(f.title, ''), coalesce(f.guid, ''),
                coalesce(b.dateAdded, 0)
         FROM moz_bookmarks b
         JOIN moz_places p ON p.id = b.fk
         LEFT JOIN moz_bookmarks f ON f.id = b.parent
         WHERE b.type = 1 AND
               coalesce(f.guid, '') <> 'tags________' AND
               coalesce(f.parent, 0) NOT IN (SELECT id FROM moz_bookmarks WHERE guid = 'tags________')
         ORDER BY b.dateAdded",
    )?;
    let bookmarks = statement
        .query_map([], |row| {
            let folder: String = row.get(2)?;
            let guid: String = row.get(3)?;
            let folder = match guid.as_str() {
                "menu________" => "Bookmarks menu".to_owned(),
                "toolbar_____" => "Bookmarks toolbar".to_owned(),
                "unfiled_____" => "Other bookmarks".to_owned(),
                "mobile______" => "Mobile bookmarks".to_owned(),
                _ => folder,
            };
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                folder,
                (row.get::<_, i64>(4)? / 1_000_000).max(0) as u64,
            ))
        })?
        .filter_map(Result::ok)
        .filter(|(url, ..)| url.starts_with("http://") || url.starts_with("https://"))
        .collect();
    // Visits from links, typed URLs, bookmarks and redirects; not embedded frames, downloads or
    // reloads. Times are in microseconds.
    let cutoff = (crate::desktop::history::now_ms() -
        crate::desktop::history::KEEP_DAYS * 24 * 60 * 60 * 1000) *
        1000;
    let mut statement = connection.prepare(
        "SELECT p.url, coalesce(p.title, ''), v.visit_date
         FROM moz_historyvisits v JOIN moz_places p ON p.id = v.place_id
         WHERE p.hidden = 0 AND v.visit_type IN (1, 2, 3, 5, 6, 8) AND v.visit_date >= ?1
         ORDER BY v.visit_date",
    )?;
    let visits = statement
        .query_map([cutoff], |row| {
            Ok(ZenVisit {
                url: row.get(0)?,
                title: row.get(1)?,
                time: row.get::<_, i64>(2)? / 1000,
            })
        })?
        .filter_map(Result::ok)
        .filter(|visit| crate::desktop::history::recordable(&visit.url))
        .collect();
    Ok(ZenPlaces { bookmarks, visits })
}
