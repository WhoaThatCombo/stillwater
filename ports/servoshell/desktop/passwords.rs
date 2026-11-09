/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Saved passwords. A small script in every page (`passwords.js`) reports sign-in forms and
//! the username and password they were submitted with; Stillwater offers to save new logins in
//! the sidebar and fills saved ones into sign-in forms on the same origin.
//!
//! Logins are kept in `<config dir>/stillwater-passwords.dat`, encrypted with Windows' DPAPI for
//! the current Windows user (like Chrome and Edge). Passwords never go through the log: the
//! page's messages are taken out before console output is printed or logged.

use std::path::PathBuf;
use std::rc::Rc;
use std::time::{SystemTime, UNIX_EPOCH};

use log::warn;
use serde_json::{Value, json};
use servo::{JSValue, UserContentManager, UserScript, WebView};
use url::Url;

/// Console messages from the page-side script start with this.
pub(crate) const MESSAGE_PREFIX: &str = "__stillwater_pw__";

/// Longest username or password accepted from a page.
const MAX_FIELD: usize = 1024;

#[derive(Clone, Debug)]
pub(crate) struct Login {
    pub(crate) id: u64,
    /// `https://example.com` (scheme, host and port).
    pub(crate) origin: String,
    pub(crate) username: String,
    pub(crate) password: String,
    /// Seconds since 1970.
    pub(crate) created: u64,
    pub(crate) last_used: u64,
    pub(crate) times_used: u64,
}

impl Login {
    fn from_json(value: &Value) -> Option<Self> {
        Some(Self {
            id: value["id"].as_u64()?,
            origin: value["origin"].as_str()?.to_owned(),
            username: value["username"].as_str().unwrap_or_default().to_owned(),
            password: value["password"].as_str()?.to_owned(),
            created: value["created"].as_u64().unwrap_or(0),
            last_used: value["lastUsed"].as_u64().unwrap_or(0),
            times_used: value["timesUsed"].as_u64().unwrap_or(0),
        })
    }

    fn to_json(&self) -> Value {
        json!({
            "id": self.id, "origin": self.origin, "username": self.username,
            "password": self.password, "created": self.created, "lastUsed": self.last_used,
            "timesUsed": self.times_used,
        })
    }
}

/// A login the user just signed in with that isn't saved yet (or changed).
struct Offer {
    origin: String,
    username: String,
    password: String,
    /// The saved login this would replace (same site and username, new password).
    replaces: Option<u64>,
}

/// What the sidebar asked to do with saved passwords.
#[derive(Clone, Debug)]
pub(crate) enum PasswordAction {
    /// Save (or update) the login offered after signing in.
    SaveOffer,
    /// Don't offer to save passwords for the offer's site again.
    NeverForSite,
    DismissOffer,
    /// Fill a saved login into the page shown in the URL bar.
    Fill(u64),
    /// Copy a login's username (false) or password (true).
    Copy(u64, bool),
    /// Send a login's password to the sidebar to show it.
    Reveal(u64),
    Delete(u64),
    /// Fill saved logins into sign-in forms when the page loads.
    SetAutofill(bool),
    /// Let a site that was set to "Never" offer to save again.
    ForgetNever(String),
}

pub(crate) struct Passwords {
    logins: Vec<Login>,
    /// Origins the user chose "Never" for.
    never: Vec<String>,
    autofill: bool,
    next_id: u64,
    path: Option<PathBuf>,
    /// Why saved passwords couldn't be read, if they couldn't.
    problem: Option<String>,
    offer: Option<Offer>,
    _script: Rc<UserScript>,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |time| time.as_secs())
}

/// The origin passwords are saved for: only for web pages, never `file:` or internal pages.
pub(crate) fn origin_of(url: &Url) -> Option<String> {
    matches!(url.scheme(), "http" | "https").then(|| url.origin().ascii_serialization())
}

/// A site's name for display: its host without `www.`.
fn host_of(origin: &str) -> String {
    Url::parse(origin)
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned))
        .map(|host| host.strip_prefix("www.").map(str::to_owned).unwrap_or(host))
        .unwrap_or_else(|| origin.to_owned())
}

/// A site's name for the save offer, with enough detail to tell look-alike sites apart: the
/// full host (no `www.` stripping), the port when it isn't the usual one, and `http://` in
/// front of sites that aren't secure.
fn offer_host_of(origin: &str) -> String {
    let Ok(url) = Url::parse(origin) else {
        return origin.to_owned();
    };
    let host = url.host_str().unwrap_or(origin);
    let host = match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_owned(),
    };
    if url.scheme() == "https" {
        host
    } else {
        format!("http://{host} (not secure)")
    }
}

/// Whether saved logins may be filled in as soon as the page shows a form. Only secure pages
/// qualify: on plain `http:` a network attacker could add a form to the page and collect the
/// filled-in password. Pages on this computer (`localhost` and loopback addresses) count as
/// secure. Other `http:` pages are filled only when the user picks a login in the sidebar.
fn fills_on_load(url: &Url) -> bool {
    match url.scheme() {
        "https" => true,
        "http" => match url.host() {
            Some(url::Host::Domain(domain)) => domain.eq_ignore_ascii_case("localhost"),
            Some(url::Host::Ipv4(address)) => address.is_loopback(),
            Some(url::Host::Ipv6(address)) => address.is_loopback(),
            None => false,
        },
        _ => false,
    }
}

impl Passwords {
    pub(crate) fn new(user_content_manager: &UserContentManager) -> Self {
        let script = Rc::new(UserScript::new(
            include_str!("passwords.js").replace("__PREFIX__", MESSAGE_PREFIX),
            None,
        ));
        user_content_manager.add_script(script.clone());
        let path =
            crate::desktop::settings::config_dir().map(|dir| dir.join("stillwater-passwords.dat"));
        let mut passwords = Self {
            logins: Vec::new(),
            never: Vec::new(),
            autofill: true,
            next_id: 1,
            path,
            problem: None,
            offer: None,
            _script: script,
        };
        passwords.load();
        passwords
    }

    fn load(&mut self) {
        let Some(path) = &self.path else {
            return;
        };
        let Ok(bytes) = std::fs::read(path) else {
            return;
        };
        let saved =
            unprotect(&bytes).and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
        let Some(saved) = saved else {
            // Encrypted for another Windows user or computer. Keep the file, out of the way.
            let aside = path.with_extension(format!("dat.unreadable-{}", now()));
            let _ = std::fs::rename(path, &aside);
            self.problem = Some(format!(
                "Saved passwords couldn't be decrypted (they belong to another Windows account). They were moved to {}.",
                aside.display()
            ));
            return;
        };
        self.logins = saved["logins"]
            .as_array()
            .map(|logins| logins.iter().filter_map(Login::from_json).collect())
            .unwrap_or_default();
        self.never = saved["never"]
            .as_array()
            .map(|list| {
                list.iter()
                    .filter_map(|origin| origin.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        self.autofill = saved["autofill"].as_bool().unwrap_or(true);
        self.next_id = self.logins.iter().map(|login| login.id).max().unwrap_or(0) + 1;
    }

    fn save(&self) {
        let Some(path) = &self.path else {
            return;
        };
        let data = json!({
            "version": 1,
            "logins": self.logins.iter().map(Login::to_json).collect::<Vec<_>>(),
            "never": self.never,
            "autofill": self.autofill,
        });
        let Some(bytes) = protect(data.to_string().as_bytes()) else {
            warn!("Could not encrypt saved passwords");
            return;
        };
        // Write a new file and swap it in, so a crash can't leave half a file.
        let temporary = path.with_extension("dat.new");
        let result = path
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|_| std::fs::write(&temporary, bytes))
            .and_then(|_| std::fs::rename(&temporary, path));
        if let Err(error) = result {
            warn!("Could not save passwords to {}: {error}", path.display());
        }
    }

    /// Saving works on this platform (passwords are only ever stored encrypted).
    pub(crate) fn available(&self) -> bool {
        cfg!(windows)
    }

    fn logins_for<'a>(&'a self, origin: &'a str) -> impl Iterator<Item = &'a Login> + 'a {
        self.logins
            .iter()
            .filter(move |login| login.origin == origin)
    }

    /// A message from the page script (`MESSAGE_PREFIX` already removed).
    pub(crate) fn on_page_message(&mut self, webview: &WebView, payload: &str) {
        let Some(url) = webview.url() else {
            return;
        };
        let Some(origin) = origin_of(&url) else {
            return;
        };
        let (kind, data) = payload.split_once('\t').unwrap_or((payload, ""));
        match kind {
            // The page shows a sign-in form: fill it if there's exactly one saved login (on
            // secure pages only, see `fills_on_load`).
            "form" if self.autofill && fills_on_load(&url) => {
                let mut logins = self.logins_for(&origin);
                if let (Some(login), None) = (logins.next(), logins.next()) {
                    fill(webview, login, false);
                }
            },
            "submit" => self.on_submit(origin, data),
            _ => {},
        }
    }

    fn on_submit(&mut self, origin: String, data: &str) {
        if !self.available() || self.never.contains(&origin) {
            return;
        }
        let Ok(value) = serde_json::from_str::<Value>(data) else {
            return;
        };
        let username = value["u"].as_str().unwrap_or_default().trim().to_owned();
        let password = value["p"].as_str().unwrap_or_default().to_owned();
        if password.is_empty() || password.len() > MAX_FIELD || username.len() > MAX_FIELD {
            return;
        }
        let same_user: Vec<&Login> = self
            .logins_for(&origin)
            .filter(|login| login.username == username || username.is_empty())
            .collect();
        if let Some(login) = same_user.iter().find(|login| login.password == password) {
            // Signed in with a saved login: just note that it was used.
            let id = login.id;
            if let Some(login) = self.logins.iter_mut().find(|login| login.id == id) {
                login.last_used = now();
                login.times_used += 1;
            }
            self.save();
            return;
        }
        // A new password for a saved username (or, on a password-only page, for the one saved
        // login) replaces it.
        let replaces = match same_user.as_slice() {
            [login] => Some(login.id),
            _ => None,
        };
        let username = match (username.is_empty(), replaces) {
            (true, Some(id)) => self
                .get(id)
                .map(|login| login.username.clone())
                .unwrap_or_default(),
            _ => username,
        };
        self.offer = Some(Offer {
            origin,
            username,
            password,
            replaces,
        });
    }

    fn get(&self, id: u64) -> Option<&Login> {
        self.logins.iter().find(|login| login.id == id)
    }

    /// Add a login (e.g. imported). Returns false if the same login was already saved.
    pub(crate) fn add(
        &mut self,
        origin: String,
        username: String,
        password: String,
        created: u64,
        last_used: u64,
        times_used: u64,
    ) -> bool {
        if self
            .logins
            .iter()
            .any(|login| login.origin == origin && login.username == username)
        {
            return false;
        }
        let id = self.next_id;
        self.next_id += 1;
        self.logins.push(Login {
            id,
            origin,
            username,
            password,
            created,
            last_used,
            times_used,
        });
        true
    }

    /// Save after a batch of `add`s.
    pub(crate) fn commit(&self) {
        self.save();
    }

    /// Act on a command from the sidebar. `page` is the page shown in the URL bar.
    /// Returns a password the sidebar asked to see: (login id, password).
    pub(crate) fn act(
        &mut self,
        action: PasswordAction,
        page: Option<&WebView>,
    ) -> Option<(u64, String)> {
        match action {
            PasswordAction::SaveOffer => {
                if let Some(offer) = self.offer.take() {
                    match offer
                        .replaces
                        .and_then(|id| self.logins.iter_mut().find(|login| login.id == id))
                    {
                        Some(login) => {
                            login.password = offer.password;
                            login.last_used = now();
                        },
                        None => {
                            let time = now();
                            self.logins.retain(|login| {
                                !(login.origin == offer.origin && login.username == offer.username)
                            });
                            self.add(offer.origin, offer.username, offer.password, time, time, 1);
                        },
                    }
                    self.save();
                }
            },
            PasswordAction::NeverForSite => {
                if let Some(offer) = self.offer.take() &&
                    !self.never.contains(&offer.origin)
                {
                    self.never.push(offer.origin);
                    self.save();
                }
            },
            PasswordAction::DismissOffer => self.offer = None,
            PasswordAction::Fill(id) => {
                if let (Some(login), Some(page)) = (self.get(id), page) {
                    fill(page, login, true);
                }
            },
            PasswordAction::Copy(id, password) => {
                if let Some(login) = self.get(id) {
                    copy_secret(if password {
                        &login.password
                    } else {
                        &login.username
                    });
                }
            },
            PasswordAction::Reveal(id) => {
                return self.get(id).map(|login| (id, login.password.clone()));
            },
            PasswordAction::Delete(id) => {
                self.logins.retain(|login| login.id != id);
                self.save();
            },
            PasswordAction::SetAutofill(enabled) => {
                self.autofill = enabled;
                self.save();
            },
            PasswordAction::ForgetNever(origin) => {
                self.never.retain(|never| *never != origin);
                self.save();
            },
        }
        None
    }

    /// What the sidebar shows: the save offer, this site's logins and the full list. Never
    /// includes passwords (the sidebar asks for one with `Reveal`).
    pub(crate) fn to_json(&self, page: Option<&Url>) -> Value {
        let origin = page.and_then(origin_of);
        let mut list: Vec<&Login> = self.logins.iter().collect();
        list.sort_by(|a, b| {
            host_of(&a.origin)
                .cmp(&host_of(&b.origin))
                .then(a.username.cmp(&b.username))
        });
        let entry = |login: &&Login| {
            json!({
                "id": login.id, "host": host_of(&login.origin), "origin": login.origin,
                "username": login.username,
            })
        };
        json!({
            "available": self.available(),
            "autofill": self.autofill,
            "problem": self.problem,
            "offer": self.offer.as_ref().map(|offer| json!({
                "host": offer_host_of(&offer.origin),
                "username": offer.username,
                "update": offer.replaces.is_some(),
            })),
            "site": origin.as_ref().map(|origin| self.logins_for(origin).map(|login| json!({
                "id": login.id, "username": login.username,
            })).collect::<Vec<_>>()).unwrap_or_default(),
            "list": list.iter().map(entry).collect::<Vec<_>>(),
            "never": self.never.iter().map(|origin| json!({ "origin": origin, "host": host_of(origin) })).collect::<Vec<_>>(),
        })
    }
}

/// Fill a login into `webview`'s page (the script checks the page is still on its origin).
fn fill(webview: &WebView, login: &Login, force: bool) {
    let json = |text: &str| serde_json::to_string(text).unwrap_or_else(|_| "\"\"".into());
    let script = include_str!("passwords-fill.js")
        .replace("__ORIGIN__", &json(&login.origin))
        .replace("__USERNAME__", &json(&login.username))
        .replace("__PASSWORD__", &json(&login.password))
        .replace("__FORCE__", if force { "true" } else { "false" });
    webview.evaluate_javascript(script, |_: Result<JSValue, _>| {});
}

/// Put a username or password on the clipboard, kept out of Windows' clipboard history and
/// cloud clipboard.
fn copy_secret(text: &str) {
    let Ok(mut clipboard) = arboard::Clipboard::new() else {
        return;
    };
    #[cfg(windows)]
    {
        use arboard::SetExtWindows;
        let _ = clipboard
            .set()
            .exclude_from_history()
            .exclude_from_cloud()
            .text(text.to_owned());
    }
    #[cfg(not(windows))]
    let _ = clipboard.set_text(text.to_owned());
}

/// Extra input for DPAPI, so other programs' DPAPI blobs and ours can't be swapped.
#[cfg(windows)]
const ENTROPY: &[u8] = b"Stillwater saved passwords";

#[cfg(windows)]
#[repr(C)]
struct DataBlob {
    size: u32,
    data: *mut u8,
}

#[cfg(windows)]
#[link(name = "crypt32")]
unsafe extern "system" {
    fn CryptProtectData(
        input: *const DataBlob,
        description: *const u16,
        entropy: *const DataBlob,
        reserved: *const std::ffi::c_void,
        prompt: *const std::ffi::c_void,
        flags: u32,
        output: *mut DataBlob,
    ) -> i32;
    fn CryptUnprotectData(
        input: *const DataBlob,
        description: *mut *mut u16,
        entropy: *const DataBlob,
        reserved: *const std::ffi::c_void,
        prompt: *const std::ffi::c_void,
        flags: u32,
        output: *mut DataBlob,
    ) -> i32;
}

#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn LocalFree(memory: *mut std::ffi::c_void) -> *mut std::ffi::c_void;
}

/// Never show a prompt (CRYPTPROTECT_UI_FORBIDDEN).
#[cfg(windows)]
const UI_FORBIDDEN: u32 = 0x1;

#[cfg(windows)]
#[expect(unsafe_code)]
fn dpapi(input: &[u8], encrypt: bool) -> Option<Vec<u8>> {
    let input = DataBlob {
        size: input.len().try_into().ok()?,
        data: input.as_ptr() as *mut u8,
    };
    let entropy = DataBlob {
        size: ENTROPY.len() as u32,
        data: ENTROPY.as_ptr() as *mut u8,
    };
    let mut output = DataBlob {
        size: 0,
        data: std::ptr::null_mut(),
    };
    // SAFETY: the blobs point at live buffers of the given sizes; DPAPI allocates `output`,
    // which is copied and then freed with LocalFree as documented.
    unsafe {
        let ok = if encrypt {
            CryptProtectData(
                &input,
                std::ptr::null(),
                &entropy,
                std::ptr::null(),
                std::ptr::null(),
                UI_FORBIDDEN,
                &mut output,
            )
        } else {
            CryptUnprotectData(
                &input,
                std::ptr::null_mut(),
                &entropy,
                std::ptr::null(),
                std::ptr::null(),
                UI_FORBIDDEN,
                &mut output,
            )
        };
        if ok == 0 || output.data.is_null() {
            return None;
        }
        let bytes = std::slice::from_raw_parts(output.data, output.size as usize).to_vec();
        LocalFree(output.data as *mut std::ffi::c_void);
        Some(bytes)
    }
}

#[cfg(windows)]
fn protect(data: &[u8]) -> Option<Vec<u8>> {
    dpapi(data, true)
}

#[cfg(windows)]
fn unprotect(data: &[u8]) -> Option<Vec<u8>> {
    dpapi(data, false)
}

// Elsewhere there's no OS key store wired up yet, so passwords are never written.
#[cfg(not(windows))]
fn protect(_: &[u8]) -> Option<Vec<u8>> {
    None
}

#[cfg(not(windows))]
fn unprotect(_: &[u8]) -> Option<Vec<u8>> {
    None
}
