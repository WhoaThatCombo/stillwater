/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Import from other browsers on this computer: the Firefox family (Zen, Firefox, LibreWolf,
//! Floorp, Waterfox; read by `zen_import`) and the Chromium family (Chrome, Edge, Opera,
//! Opera GX, Brave, Vivaldi; read by `chromium_import`), plus passwords exported to a CSV file.
//!
//! `STILLWATER_BROWSERS_DIR` stands in for every browser's data folder (`<dir>/<id>`, e.g.
//! `<dir>/chrome`), so tests never look at the real ones.

use std::path::{Path, PathBuf};

use crate::desktop::zen_import::{self, ZenCookie, ZenLogin, ZenPlaces, ZenProfile, ZenSession};

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Family {
    Firefox,
    Chromium,
}

/// A browser Stillwater knows how to import from.
pub(crate) struct Browser {
    pub(crate) id: &'static str,
    pub(crate) name: &'static str,
    pub(crate) family: Family,
    pub(crate) root: PathBuf,
}

/// Where each browser keeps its data, in the order they're offered.
fn known() -> Vec<(&'static str, &'static str, Family, Option<PathBuf>)> {
    let roaming = dirs::config_dir();
    let local = dirs::data_local_dir();
    let at = |base: &Option<PathBuf>, path: &str| {
        base.as_ref().map(|base| path.split('/').fold(base.clone(), |dir, part| dir.join(part)))
    };
    let windows = cfg!(windows);
    let only_windows = |path: Option<PathBuf>| if windows { path } else { None };
    let firefox = if cfg!(target_os = "macos") {
        at(&roaming, "Firefox")
    } else if windows {
        at(&roaming, "Mozilla/Firefox")
    } else {
        dirs::home_dir().map(|home| home.join(".mozilla").join("firefox"))
    };
    use Family::*;
    vec![
        ("chrome", "Chrome", Chromium, only_windows(at(&local, "Google/Chrome/User Data"))),
        ("edge", "Edge", Chromium, only_windows(at(&local, "Microsoft/Edge/User Data"))),
        ("firefox", "Firefox", Firefox, firefox),
        ("zen", "Zen", Firefox, zen_import::zen_dir()),
        ("operagx", "Opera GX", Chromium, only_windows(at(&roaming, "Opera Software/Opera GX Stable"))),
        ("opera", "Opera", Chromium, only_windows(at(&roaming, "Opera Software/Opera Stable"))),
        ("brave", "Brave", Chromium, only_windows(at(&local, "BraveSoftware/Brave-Browser/User Data"))),
        ("vivaldi", "Vivaldi", Chromium, only_windows(at(&local, "Vivaldi/User Data"))),
        ("librewolf", "LibreWolf", Firefox, only_windows(at(&roaming, "librewolf"))),
        ("floorp", "Floorp", Firefox, only_windows(at(&roaming, "Floorp"))),
        ("waterfox", "Waterfox", Firefox, only_windows(at(&roaming, "Waterfox"))),
    ]
}

/// The browsers whose data is on this computer, with their profiles.
pub(crate) fn find_browsers() -> Vec<(Browser, Vec<ZenProfile>)> {
    let test_dir = std::env::var_os("STILLWATER_BROWSERS_DIR").map(PathBuf::from);
    known()
        .into_iter()
        .filter_map(|(id, name, family, root)| {
            // Zen's folder follows its own override (`zen_dir`) too.
            let root = match &test_dir {
                Some(dir) if id != "zen" => dir.join(id),
                _ => root?,
            };
            let browser = Browser {
                id,
                name,
                family,
                root,
            };
            let profiles = profiles(&browser);
            (!profiles.is_empty()).then_some((browser, profiles))
        })
        .collect()
}

fn profiles(browser: &Browser) -> Vec<ZenProfile> {
    if !browser.root.is_dir() {
        return Vec::new();
    }
    match browser.family {
        Family::Firefox => zen_import::find_profiles_in(&browser.root),
        Family::Chromium => crate::desktop::chromium_import::find_profiles(&browser.root),
    }
}

/// What the user ticked.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Choice {
    pub(crate) pins: bool,
    pub(crate) tabs: bool,
    pub(crate) passwords: bool,
    pub(crate) history: bool,
    pub(crate) bookmarks: bool,
    pub(crate) cookies: bool,
}

impl Choice {
    /// From a comma-separated list: pins, tabs, passwords, history, bookmarks, cookies.
    pub(crate) fn parse(what: &str) -> Self {
        let what: Vec<&str> = what.split(',').collect();
        Self {
            pins: what.contains(&"pins"),
            tabs: what.contains(&"tabs"),
            passwords: what.contains(&"passwords"),
            history: what.contains(&"history"),
            bookmarks: what.contains(&"bookmarks"),
            cookies: what.contains(&"cookies"),
        }
    }
}

/// Everything read from one profile of any browser.
pub(crate) struct ImportData {
    /// The browser's name, for what's reported.
    pub(crate) browser: String,
    /// Tabs (Firefox family only; Chromium's session files aren't read).
    pub(crate) session: Option<Result<ZenSession, String>>,
    pub(crate) places: Result<ZenPlaces, String>,
    pub(crate) logins: Result<Vec<ZenLogin>, String>,
    pub(crate) cookies: Result<Vec<ZenCookie>, String>,
    pub(crate) skipped_logins: usize,
    /// Why logins were skipped, for the result line.
    pub(crate) skipped_logins_why: &'static str,
    /// Chromium: passwords and cookies under app-bound encryption, never read.
    pub(crate) protected_logins: usize,
    pub(crate) protected_cookies: usize,
    /// Chromium: cookies that couldn't be decrypted, and session cookies (not imported).
    pub(crate) unreadable_cookies: usize,
    pub(crate) session_cookies: usize,
}

const FIREFOX_SKIPPED_WHY: &str = "sign-ins for HTTP authentication, or unreadable";

impl ImportData {
    pub(crate) fn from_firefox(data: zen_import::ZenData, browser: &str) -> Self {
        Self {
            browser: browser.to_owned(),
            session: Some(data.session),
            places: data.places,
            logins: data.logins,
            cookies: data.cookies,
            skipped_logins: data.skipped_logins,
            skipped_logins_why: FIREFOX_SKIPPED_WHY,
            protected_logins: 0,
            protected_cookies: 0,
            unreadable_cookies: 0,
            session_cookies: 0,
        }
    }
}

/// Read profile number `profile` of the browser `id`: (browser, profile name, data).
pub(crate) fn read(id: &str, profile: usize) -> Option<(Browser, String, ImportData)> {
    let (browser, profiles) = find_browsers()
        .into_iter()
        .find(|(browser, _)| browser.id == id)?;
    let chosen = profiles.into_iter().nth(profile)?;
    let data = match browser.family {
        Family::Firefox => ImportData::from_firefox(
            zen_import::read_profile_as(&chosen.path, browser.name),
            browser.name,
        ),
        Family::Chromium => {
            let data =
                crate::desktop::chromium_import::read_profile(&browser.root, &chosen.path, browser.name);
            ImportData {
                browser: browser.name.to_owned(),
                session: None,
                places: data.places,
                logins: data.logins,
                cookies: data.cookies,
                skipped_logins: data.skipped_logins,
                skipped_logins_why: "sign-ins for apps or HTTP authentication, or unreadable",
                protected_logins: data.protected_logins,
                protected_cookies: data.protected_cookies,
                unreadable_cookies: data.unreadable_cookies,
                session_cookies: data.session_cookies,
            }
        },
    };
    Some((browser, chosen.name, data))
}

/// The browsers found and what profile `profile` of browser `id` (or the first browser) holds,
/// for the import section in settings.
pub(crate) fn scan(id: &str, profile: usize) -> serde_json::Value {
    let browsers = find_browsers();
    let list: Vec<serde_json::Value> = browsers
        .iter()
        .map(|(browser, profiles)| {
            serde_json::json!({
                "id": browser.id,
                "name": browser.name,
                "profiles": profiles.iter().map(|profile| profile.name.as_str()).collect::<Vec<_>>(),
            })
        })
        .collect();
    let Some((browser, profiles)) = browsers
        .iter()
        .find(|(browser, _)| browser.id == id)
        .or(browsers.first())
    else {
        return serde_json::json!({ "browsers": list });
    };
    let profile = if browser.id == id { profile.min(profiles.len() - 1) } else { 0 };
    let Some((_, _, data)) = read(browser.id, profile) else {
        return serde_json::json!({ "browsers": list });
    };
    let session = data.session.as_ref().and_then(|session| session.as_ref().ok());
    let count = |test: fn(&zen_import::ZenTab) -> bool| {
        session.map_or(0, |session| session.tabs.iter().filter(|tab| test(tab)).count())
    };
    serde_json::json!({
        "browsers": list,
        "browser": browser.id,
        "name": browser.name,
        "profile": profile,
        // What this browser can give: Zen has spaces, Firefox only pinned tabs, the Chromium
        // family no tabs at all.
        "spacesKind": (if browser.id == "zen" { "zen" } else { "pins" }),
        "hasTabs": data.session.is_some(),
        "spaces": session.map_or(0, |session| session.spaces.len()),
        "pins": count(|tab| tab.pinned && !tab.essential),
        "essentials": count(|tab| tab.essential),
        "tabs": count(|tab| !tab.pinned && !tab.essential),
        "sessionProblem": data.session.as_ref().and_then(|session| session.as_ref().err()),
        "passwords": data.logins.as_ref().map_or(0, Vec::len),
        "protectedPasswords": data.protected_logins,
        "passwordProblem": data.logins.as_ref().err(),
        "bookmarks": data.places.as_ref().map_or(0, |places| places.bookmarks.len()),
        "visits": data.places.as_ref().map_or(0, |places| places.visits.len()),
        "placesProblem": data.places.as_ref().err(),
        "cookies": data.cookies.as_ref().map_or(0, Vec::len),
        "protectedCookies": data.protected_cookies,
        "cookiesProblem": data.cookies.as_ref().err(),
    })
}

/// The note shown when some of a Chromium browser's data is under app-bound encryption.
pub(crate) fn protected_note(browser: &str, passwords: usize, cookies: usize) -> Option<String> {
    let plural = |count: usize, one: &str, many: &str| {
        format!("{count} {}", if count == 1 { one } else { many })
    };
    let what = match (passwords, cookies) {
        (0, 0) => return None,
        (passwords, 0) => plural(passwords, "password", "passwords"),
        (0, cookies) => plural(cookies, "cookie", "cookies"),
        (passwords, cookies) => format!(
            "{} and {}",
            plural(passwords, "password", "passwords"),
            plural(cookies, "cookie", "cookies")
        ),
    };
    let verb = if passwords + cookies == 1 { "is" } else { "are" };
    let mut note = format!(
        "{what} {verb} protected by {browser} so that other programs can't read them, and weren't imported."
    );
    if passwords > 0 {
        note.push_str(&format!(
            " To bring those passwords over, export them in {browser}'s password settings (Export passwords) and use \"Import passwords from a file\" below."
        ));
    }
    if cookies > 0 {
        note.push_str(" For those cookies, sign in to the sites again.");
    }
    Some(note)
}

// ---- Passwords from a CSV file ----

/// Rows of a CSV file (RFC 4180: quoted fields may hold commas, quotes and line breaks).
fn parse_csv(text: &str) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut chars = text.trim_start_matches('\u{feff}').chars().peekable();
    while let Some(c) = chars.next() {
        if quoted {
            match c {
                '"' if chars.peek() == Some(&'"') => {
                    chars.next();
                    field.push('"');
                },
                '"' => quoted = false,
                _ => field.push(c),
            }
            continue;
        }
        match c {
            '"' => quoted = true,
            ',' => row.push(std::mem::take(&mut field)),
            '\r' => {},
            '\n' => {
                row.push(std::mem::take(&mut field));
                rows.push(std::mem::take(&mut row));
            },
            _ => field.push(c),
        }
    }
    if !field.is_empty() || !row.is_empty() {
        row.push(field);
        rows.push(row);
    }
    rows.retain(|row| row.iter().any(|field| !field.is_empty()));
    rows
}

/// Logins from a password CSV exported by Chrome, Edge, Opera, Brave (`name,url,username,
/// password,note`), Firefox (`url,username,password,httpRealm,...,timeCreated,...`) or a
/// password manager using similar column names: (logins, rows skipped).
pub(crate) fn read_password_csv(text: &str) -> Result<(Vec<ZenLogin>, usize), String> {
    let rows = parse_csv(text);
    let Some((header, rows)) = rows.split_first() else {
        return Err("The file is empty.".to_owned());
    };
    let column = |names: &[&str]| {
        header
            .iter()
            .position(|title| names.iter().any(|name| title.trim().eq_ignore_ascii_case(name)))
    };
    let (Some(url), Some(username), Some(password)) = (
        column(&["url", "origin", "login_uri", "website", "web site"]),
        column(&["username", "login_username", "user name"]),
        column(&["password", "login_password"]),
    ) else {
        return Err(
            "That file doesn't look like a password export: it needs url, username and password columns."
                .to_owned(),
        );
    };
    let realm = column(&["httpRealm"]);
    let created = column(&["timeCreated"]);
    let last_used = column(&["timeLastUsed"]);
    let get = |row: &Vec<String>, index: Option<usize>| {
        index
            .and_then(|index| row.get(index))
            .map(|field| field.trim().to_owned())
            .unwrap_or_default()
    };
    // Firefox writes times in milliseconds since 1970.
    let seconds = |text: String| text.parse::<u64>().map_or(0, |ms| ms / 1000);
    let mut logins: Vec<ZenLogin> = Vec::new();
    let mut skipped = 0;
    for row in rows {
        let origin = url::Url::parse(&get(row, Some(url)))
            .ok()
            .filter(|url| matches!(url.scheme(), "http" | "https") && url.host().is_some())
            .map(|url| url.origin().ascii_serialization());
        let password_value = row.get(password).cloned().unwrap_or_default();
        let Some(origin) = origin.filter(|_| !password_value.is_empty() && get(row, realm).is_empty())
        else {
            skipped += 1;
            continue;
        };
        logins.push(ZenLogin {
            origin,
            username: get(row, Some(username)),
            password: password_value,
            created: seconds(get(row, created)),
            last_used: seconds(get(row, last_used)),
            times_used: 0,
        });
    }
    Ok((logins, skipped))
}

/// A file picked in "Import passwords from a file": its name (for the result line) and text.
pub(crate) fn read_text_file(path: &Path) -> Result<String, String> {
    let bytes = std::fs::read(path).map_err(|_| "The file couldn't be read.".to_owned())?;
    // Password exports are small; refuse something that clearly isn't one.
    if bytes.len() > 64 * 1024 * 1024 {
        return Err("That file is too big to be a password export.".to_owned());
    }
    String::from_utf8(bytes).map_err(|_| "That file isn't a text (CSV) file.".to_owned())
}
