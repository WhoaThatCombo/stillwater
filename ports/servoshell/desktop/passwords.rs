/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Saved passwords. A small script in every page (`passwords.js`) reports sign-in forms,
//! focused sign-in fields and the username and password a form was submitted with; Stillwater
//! offers to save new logins in the sidebar and fills saved ones into sign-in forms of the same
//! site (see `relation`): logins saved for the page's origin, for the same host with or without
//! `www.` and for the `http:` version of an `https:` page are filled like the page's own; logins
//! saved for another host of the same site (github.com on gist.github.com) are only listed,
//! labelled with their host, and filled when picked. Nothing is offered across sites, or from an
//! `https:` login on an `http:` page. With one fillable login it's filled in as the form
//! appears; with several, nothing is filled until the user picks one from the list Stillwater
//! draws under the focused field (`draw_chooser`). Only top-level pages are filled, never frames.
//!
//! Why something was or wasn't offered goes to the diagnostics log (host and counts only, never
//! usernames or passwords).
//!
//! Logins are kept in `<config dir>/stillwater-passwords.dat`, encrypted with Windows' DPAPI for
//! the current Windows user (like Chrome and Edge). Passwords never go through the log: the
//! page's messages are taken out before console output is printed or logged.

use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use log::warn;
use serde_json::{Value, json};
use servo::{JSValue, UserContentManager, UserScript, WebView, WebViewId};
use url::Url;

/// Console messages from the page-side script start with this.
pub(crate) const MESSAGE_PREFIX: &str = "__stillwater_pw__";

/// How long a login picked from the list on a username-first page is remembered for the
/// password page that follows.
const PICKED_LOGIN_LIFETIME: Duration = Duration::from_secs(5 * 60);

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
            origin: normalize_origin(value["origin"].as_str()?),
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
    /// Fill saved logins into sign-in forms when the page loads, and list them under focused
    /// sign-in fields.
    SetAutofill(bool),
    /// Tests: press a key in the saved-login list ("down", "up", "enter", "escape").
    ChooserKey(String),
    /// Let a site that was set to "Never" offer to save again.
    ForgetNever(String),
}

/// How a saved login relates to the page it might be offered on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Match {
    /// Saved for exactly the page's origin.
    Exact,
    /// Saved for the same host with or without `www.`, or for `http:` while the page is the
    /// `https:` version. Filled like an exact match.
    SameHost,
    /// Saved for another host of the same site (same registrable domain, e.g. github.com on
    /// gist.github.com). Only listed, with its host, and filled when the user picks it.
    OtherHost,
}

/// `origin` the way `origin_of` writes it (lowercase host, no default port, no trailing slash),
/// or unchanged if it isn't a web origin. Imported logins come from other browsers.
pub(crate) fn normalize_origin(origin: &str) -> String {
    Url::parse(origin.trim())
        .ok()
        .as_ref()
        .and_then(origin_of)
        .unwrap_or_else(|| origin.to_owned())
}

/// Whether, and how, a login saved for `saved` may be offered on `page`.
fn relation(saved: &str, page: &Url) -> Option<Match> {
    let page_origin = origin_of(page)?;
    if saved == page_origin {
        return Some(Match::Exact);
    }
    let saved = Url::parse(saved).ok()?;
    // A login saved for a secure page is never offered on an insecure one (a network attacker
    // could add the form); one saved for `http:` may be used on `https:`.
    let scheme_ok = saved.scheme() == page.scheme() ||
        (saved.scheme() == "http" && page.scheme() == "https");
    // `port()` is None for the scheme's usual port, so http:80 and https:443 count as the same.
    if !scheme_ok || saved.port() != page.port() {
        return None;
    }
    // Addresses and single-label names (localhost) only ever match exactly, or as the http:
    // version of the same address.
    let (Some(url::Host::Domain(saved_host)), Some(url::Host::Domain(page_host))) =
        (saved.host(), page.host())
    else {
        return (saved.host() == page.host()).then_some(Match::SameHost);
    };
    let saved_host = saved_host.to_ascii_lowercase();
    let page_host = page_host.to_ascii_lowercase();
    let bare = |host: &str| host.strip_prefix("www.").unwrap_or(host).to_owned();
    if bare(&saved_host) == bare(&page_host) {
        return Some(Match::SameHost);
    }
    // The same registrable domain ("site"), from the public suffix list: never across sites,
    // and never for hosts on a shared suffix (alice.github.io and bob.github.io are different
    // sites).
    let page_site = servo::reg_suffix(&page_host).to_ascii_lowercase();
    let saved_site = servo::reg_suffix(&saved_host).to_ascii_lowercase();
    (page_site == saved_site && servo::is_reg_domain(&page_site)).then_some(Match::OtherHost)
}

/// The list of saved logins shown under a focused sign-in field. It's drawn by Stillwater over
/// the page, so the page never sees which logins are saved; only the one picked is filled in.
pub(crate) struct Chooser {
    pub(crate) webview: WebViewId,
    /// The origin the list is for: the page's origin when the field was focused.
    pub(crate) origin: String,
    /// The field, in CSS pixels from the top left of the page's viewport.
    pub(crate) field: [f32; 4],
    /// Login ids, usernames and, for logins saved for another host of the site (or the `http:`
    /// version), that host: the page's own logins first, then most recently used first.
    pub(crate) entries: Vec<(u64, String, Option<String>)>,
    /// The entry highlighted with the arrow keys.
    pub(crate) selected: Option<usize>,
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
    chooser: Option<Chooser>,
    /// The login picked from the list in each tab, with its origin and when: when a
    /// username-first page then asks for the password, it's that login's.
    picked: HashMap<WebViewId, (String, u64, Instant)>,
    /// The last diagnostics line written for each tab, so repeated reports don't flood the log.
    last_note: HashMap<WebViewId, String>,
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
            include_str!("passwords.js")
                .replace("__DETECT__", DETECT)
                .replace("__PREFIX__", MESSAGE_PREFIX),
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
            chooser: None,
            picked: HashMap::new(),
            last_note: HashMap::new(),
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
            // Incognito leaves the profile's files alone.
            if crate::desktop::incognito::active() {
                return;
            }
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
        // Incognito: saved logins are offered for filling, but nothing is ever saved.
        let Some(path) = self.path.as_ref().filter(|_| !crate::desktop::incognito::active()) else {
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

    /// Logins saved for exactly `origin`.
    fn logins_for<'a>(&'a self, origin: &'a str) -> impl Iterator<Item = &'a Login> + 'a {
        self.logins
            .iter()
            .filter(move |login| login.origin == origin)
    }

    /// The logins that may be offered on `page`, the page's own first, then the most recently
    /// used first.
    fn matches_for(&self, page: &Url) -> Vec<(&Login, Match)> {
        let mut matches: Vec<(&Login, Match)> = self
            .logins
            .iter()
            .filter_map(|login| Some((login, relation(&login.origin, page)?)))
            .collect();
        matches.sort_by(|(a, a_match), (b, b_match)| {
            a_match
                .cmp(b_match)
                .then_with(|| b.last_used.cmp(&a.last_used))
                .then_with(|| a.username.cmp(&b.username))
        });
        matches
    }

    /// Whether login `id` may be filled into `page`.
    fn matching(&self, id: u64, page: &Url) -> Option<&Login> {
        self.get(id)
            .filter(|login| relation(&login.origin, page).is_some())
    }

    /// Write an autofill decision to the diagnostics log, unless it's the same as the last one
    /// for this tab. Callers only ever pass hosts and counts.
    fn note(&mut self, webview: WebViewId, line: String) {
        if self.last_note.get(&webview) != Some(&line) {
            crate::desktop::diagnostics::log(&line);
            self.last_note.insert(webview, line);
        }
    }

    /// A message from the page script (`MESSAGE_PREFIX` already removed). Any page can send
    /// these (and a frame's console messages arrive here too), so they're only hints: the
    /// origin always comes from the tab's URL, the page never gets the list of saved logins,
    /// and a password is only filled into the tab's top-level page on an origin it may be
    /// offered on (`relation`).
    /// Returns true if the list under the focused field changed (the window must redraw).
    pub(crate) fn on_page_message(&mut self, webview: &WebView, payload: &str) -> bool {
        let Some(url) = webview.url() else {
            return false;
        };
        let Some(origin) = origin_of(&url) else {
            return self.close_chooser(webview.id());
        };
        if self
            .picked
            .get(&webview.id())
            .is_some_and(|(picked_origin, _, when)| {
                *picked_origin != origin || when.elapsed() > PICKED_LOGIN_LIFETIME
            })
        {
            self.picked.remove(&webview.id());
        }
        let (kind, data) = payload.split_once('\t').unwrap_or((payload, ""));
        match kind {
            "form" if !self.autofill => {
                let line = format!("passwords: sign-in form on {origin}: not filled (filling saved passwords is off)");
                self.note(webview.id(), line);
                false
            },
            // The page shows a sign-in form: fill it if there's exactly one login saved for the
            // page's own host (on secure pages only, see `fills_on_load`), or the second step of
            // a sign-in that started with a login picked from the list.
            "form" => {
                let picked = self
                    .picked
                    .get(&webview.id())
                    .and_then(|(_, id, _)| self.matching(*id, &url));
                let matches = self.matches_for(&url);
                let own: Vec<&Login> = matches
                    .iter()
                    .filter(|(_, how)| *how <= Match::SameHost)
                    .map(|(login, _)| *login)
                    .collect();
                let others = matches.len() - own.len();
                let decision = match (own.as_slice(), picked) {
                    ([login], _) if fills_on_load(&url) => {
                        fill(webview, &origin, login, "load");
                        "filled the saved login".to_owned()
                    },
                    (_, Some(login)) => {
                        fill(webview, &origin, login, "continue");
                        "continued with the login picked on the previous step".to_owned()
                    },
                    ([_], _) => "not filled: the page isn't secure (pick it from the list)".to_owned(),
                    ([], _) if others > 0 => "not filled: only logins for other hosts of this site (listed on focus)".to_owned(),
                    ([], _) => {
                        let sites = self.logins.len();
                        format!("nothing to fill: no saved login for this site ({sites} saved for other sites)")
                    },
                    _ => "not filled: several saved logins (listed on focus)".to_owned(),
                };
                let line = format!(
                    "passwords: sign-in form on {origin}: {} for this host, {others} for other hosts of the site; {decision}",
                    own.len(),
                );
                self.note(webview.id(), line);
                false
            },
            // What the page script found (counts only), for the diagnostics log.
            "scan" => {
                let value: Value = serde_json::from_str(data).unwrap_or_default();
                let count = |key: &str| value[key].as_u64().unwrap_or(0);
                let line = format!(
                    "passwords: {origin}: {} password field(s) ({} in shadow DOM), {} sign-in form(s), {} username-only step(s){}",
                    count("passwords"),
                    count("shadow"),
                    count("groups"),
                    count("userOnly"),
                    if count("passwords") > 0 && count("groups") == 0 {
                        " - password field not recognised as a sign-in form"
                    } else {
                        ""
                    },
                );
                self.note(webview.id(), line);
                false
            },
            "focus" if self.autofill => {
                let Some(field) = parse_rect(data) else {
                    return false;
                };
                let matches = self.matches_for(&url);
                if matches.is_empty() {
                    let line = format!("passwords: sign-in field focused on {origin}: no saved login for this site");
                    self.note(webview.id(), line);
                    return self.close_chooser(webview.id());
                }
                let entries: Vec<_> = matches
                    .iter()
                    .map(|(login, how)| {
                        let label = (*how != Match::Exact).then(|| offer_host_of(&login.origin));
                        (login.id, login.username.clone(), label)
                    })
                    .collect();
                let line = format!(
                    "passwords: sign-in field focused on {origin}: listing {} saved login(s)",
                    entries.len()
                );
                self.note(webview.id(), line);
                self.chooser = Some(Chooser {
                    webview: webview.id(),
                    origin,
                    field,
                    entries,
                    selected: None,
                });
                true
            },
            "move" => match (&mut self.chooser, parse_rect(data)) {
                (Some(chooser), Some(field)) if chooser.webview == webview.id() => {
                    chooser.field = field;
                    true
                },
                _ => false,
            },
            "blur" => self.close_chooser(webview.id()),
            "submit" => {
                self.on_submit(origin, &url, data);
                false
            },
            _ => false,
        }
    }

    /// The list under the focused field, if it's showing in `webview` on `url`'s origin.
    pub(crate) fn chooser_for(&self, webview: &WebView) -> Option<&Chooser> {
        let chooser = self.chooser.as_ref()?;
        let origin = webview.url().as_ref().and_then(origin_of)?;
        (chooser.webview == webview.id() && chooser.origin == origin).then_some(chooser)
    }

    /// Close the list if it's showing in `webview`. Returns true if it was.
    pub(crate) fn close_chooser(&mut self, webview: WebViewId) -> bool {
        if self
            .chooser
            .as_ref()
            .is_some_and(|chooser| chooser.webview == webview)
        {
            self.chooser = None;
            return true;
        }
        false
    }

    /// Fill the list's entry `index` into the focused field's form in `webview`.
    pub(crate) fn pick(&mut self, webview: &WebView, index: usize) {
        let Some(chooser) = self.chooser.take() else {
            return;
        };
        let Some(&(id, ..)) = chooser.entries.get(index) else {
            return;
        };
        // The page must still be on the origin the list was shown for.
        let Some(url) = webview.url() else {
            return;
        };
        if chooser.webview != webview.id() || origin_of(&url).as_ref() != Some(&chooser.origin) {
            return;
        }
        let Some(login) = self.matching(id, &url) else {
            return;
        };
        fill(webview, &chooser.origin, login, "focused");
        self.picked
            .insert(webview.id(), (chooser.origin.clone(), id, Instant::now()));
    }

    /// A key pressed while the list shows in `webview`: the arrow keys move the highlight, Enter
    /// fills the highlighted login and Escape closes the list. Returns true if the key was used
    /// (and must not reach the page). Any other key closes the list and goes to the page.
    pub(crate) fn chooser_key(&mut self, webview: &WebView, key: &str) -> bool {
        let Some(chooser) = self.chooser.as_mut() else {
            return false;
        };
        let count = chooser.entries.len();
        if chooser.webview != webview.id() || count == 0 {
            return false;
        }
        match key {
            "down" => {
                chooser.selected = Some(chooser.selected.map_or(0, |index| (index + 1) % count));
                true
            },
            "up" => {
                chooser.selected = Some(
                    chooser
                        .selected
                        .map_or(count - 1, |index| (index + count - 1) % count),
                );
                true
            },
            "enter" => match chooser.selected {
                Some(index) => {
                    self.pick(webview, index);
                    true
                },
                // Nothing highlighted: Enter submits the form as usual.
                None => {
                    self.chooser = None;
                    false
                },
            },
            "escape" => {
                self.chooser = None;
                true
            },
            _ => {
                self.chooser = None;
                false
            },
        }
    }

    fn on_submit(&mut self, origin: String, url: &Url, data: &str) {
        // Incognito: no offer to save, and saved logins' use isn't recorded.
        if !self.available() || self.never.contains(&origin) || crate::desktop::incognito::active() {
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
        // Signed in with a login saved for this site (also one saved for another host of it,
        // e.g. picked from the list): no need to save it again.
        let used = self
            .matches_for(url)
            .into_iter()
            .map(|(login, _)| login)
            .find(|login| {
                login.password == password && (login.username == username || username.is_empty())
            });
        if let Some(login) = used {
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
        // Other browsers' origins may differ in case, a trailing slash or a default port.
        let origin = normalize_origin(&origin);
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
                if let Some(page) = page &&
                    let Some(url) = page.url() &&
                    let Some(origin) = origin_of(&url) &&
                    let Some(login) = self.matching(id, &url)
                {
                    fill(page, &origin, login, "page");
                }
            },
            PasswordAction::ChooserKey(key) => {
                if let Some(page) = page {
                    self.chooser_key(page, &key);
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
                if !enabled {
                    self.chooser = None;
                    self.picked.clear();
                }
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
            "site": page.map(|page| self.matches_for(page).iter().map(|(login, how)| json!({
                "id": login.id, "username": login.username,
                "host": (*how != Match::Exact).then(|| offer_host_of(&login.origin)),
            })).collect::<Vec<_>>()).unwrap_or_default(),
            "list": list.iter().map(entry).collect::<Vec<_>>(),
            "never": self.never.iter().map(|origin| json!({ "origin": origin, "host": host_of(origin) })).collect::<Vec<_>>(),
            // For tests: whether the list under a focused field shows, and how long it is.
            "chooser": self.chooser.as_ref().map(|chooser| json!({
                "count": chooser.entries.len(), "selected": chooser.selected,
                "labels": chooser.entries.iter().map(|(.., label)| label.clone()).collect::<Vec<_>>(),
            })),
        })
    }
}

/// Draw the saved-login list under its field, over `content_rect` (where the page is shown,
/// `zoom` times CSS pixels). Returns where it was drawn and the entry clicked, if any.
pub(crate) fn draw_chooser(
    ctx: &egui::Context,
    chooser: &Chooser,
    content_rect: egui::Rect,
    zoom: f32,
    theme: servo::Theme,
) -> (egui::Rect, Option<usize>) {
    use egui::{Color32, CornerRadius, Id, Order, Sense, Stroke, StrokeKind, pos2, vec2};

    // The same palette as the find bar.
    let (fill, hover, foreground, muted) = match theme {
        servo::Theme::Dark => (
            Color32::from_rgb(43, 42, 51),
            Color32::from_rgb(62, 61, 72),
            Color32::from_rgb(235, 235, 240),
            Color32::from_rgb(160, 160, 170),
        ),
        servo::Theme::Light => (
            Color32::from_rgb(250, 250, 252),
            Color32::from_rgb(232, 232, 238),
            Color32::from_rgb(30, 30, 36),
            Color32::from_rgb(105, 105, 115),
        ),
    };
    let accent = Color32::from_rgb(255, 150, 50);
    const HEADER: f32 = 26.0;
    const ROW: f32 = 40.0;
    const PADDING: f32 = 4.0;
    const MAX_ROWS: usize = 6;

    let [x, y, width, height] = chooser.field.map(|value| value * zoom);
    let rows = chooser.entries.len().min(MAX_ROWS) as f32;
    let size = vec2(
        width.max(260.0).min(content_rect.width()),
        HEADER + rows * ROW + PADDING * 2.0,
    );
    // Under the field, or above it when there's no room below; always inside the page.
    let below = content_rect.min.y + y + height + 2.0;
    let top = if below + size.y > content_rect.max.y {
        content_rect.min.y + y - size.y - 2.0
    } else {
        below
    };
    let min = pos2(
        (content_rect.min.x + x).clamp(content_rect.min.x, (content_rect.max.x - size.x).max(content_rect.min.x)),
        top.clamp(content_rect.min.y, (content_rect.max.y - size.y).max(content_rect.min.y)),
    );
    let rect = egui::Rect::from_min_size(min, size);
    let host = host_of(&chooser.origin);

    let mut picked = None;
    egui::Area::new(Id::new("password-chooser"))
        .order(Order::Foreground)
        .fixed_pos(rect.min)
        .show(ctx, |ui| {
            let painter = ui.painter().clone();
            painter.rect_filled(
                rect.translate(vec2(0.0, 2.0)).expand(1.0),
                CornerRadius::same(12),
                Color32::from_black_alpha(40),
            );
            painter.rect_filled(rect, CornerRadius::same(10), fill);
            painter.rect_stroke(
                rect,
                CornerRadius::same(10),
                Stroke::new(1.0_f32, Color32::from_black_alpha(40)),
                StrokeKind::Inside,
            );
            ui.interact(rect, Id::new("password-chooser-background"), Sense::click());
            painter.text(
                pos2(rect.min.x + 12.0, rect.min.y + PADDING + HEADER / 2.0),
                egui::Align2::LEFT_CENTER,
                format!("Saved passwords for {host}"),
                egui::FontId::proportional(11.5),
                muted,
            );
            let list = egui::Rect::from_min_max(
                pos2(rect.min.x + PADDING, rect.min.y + PADDING + HEADER),
                pos2(rect.max.x - PADDING, rect.max.y - PADDING),
            );
            let mut child = ui.new_child(egui::UiBuilder::new().max_rect(list));
            egui::ScrollArea::vertical()
                .max_height(list.height())
                .show(&mut child, |ui| {
                    for (index, (_, username, label)) in chooser.entries.iter().enumerate() {
                        let (row, response) =
                            ui.allocate_exact_size(vec2(list.width(), ROW), Sense::click());
                        let selected = chooser.selected == Some(index);
                        if selected || response.hovered() {
                            ui.painter().rect_filled(row, CornerRadius::same(7), hover);
                        }
                        if selected {
                            ui.scroll_to_rect(row, None);
                        }
                        let painter = ui.painter().with_clip_rect(row.intersect(list));
                        // A key-coloured dot, then the username and a masked password.
                        let dot = pos2(row.min.x + 18.0, row.center().y);
                        painter.circle_filled(dot, 11.0, accent.gamma_multiply(0.18));
                        painter.text(
                            dot,
                            egui::Align2::CENTER_CENTER,
                            username.chars().next().map_or("•".to_owned(), |c| c.to_uppercase().to_string()),
                            egui::FontId::proportional(12.0),
                            accent,
                        );
                        let name = if username.is_empty() {
                            "(no username)"
                        } else {
                            username
                        };
                        painter.text(
                            pos2(row.min.x + 38.0, row.min.y + 13.0),
                            egui::Align2::LEFT_CENTER,
                            name,
                            egui::FontId::proportional(13.5),
                            foreground,
                        );
                        // A login saved for another host of the site says which.
                        let detail = match label {
                            Some(label) => format!("••••••••  ·  saved for {label}"),
                            None => "••••••••".to_owned(),
                        };
                        painter.text(
                            pos2(row.min.x + 38.0, row.min.y + 29.0),
                            egui::Align2::LEFT_CENTER,
                            detail,
                            egui::FontId::proportional(11.0),
                            muted,
                        );
                        if response.clicked() {
                            picked = Some(index);
                        }
                    }
                });
        });
    (rect, picked)
}

/// The sign-in field finder shared by `passwords.js` and `passwords-fill.js`.
const DETECT: &str = include_str!("passwords-detect.js");

/// `{"x":…,"y":…,"w":…,"h":…}` from the page: a field's rectangle in CSS pixels.
fn parse_rect(data: &str) -> Option<[f32; 4]> {
    let value: Value = serde_json::from_str(data).ok()?;
    let number = |key: &str| {
        value[key]
            .as_f64()
            .filter(|number| number.is_finite())
            .map(|number| number.clamp(-100_000.0, 100_000.0) as f32)
    };
    Some([number("x")?, number("y")?, number("w")?, number("h")?])
}

/// Fill a login into `webview`'s top-level page (the script checks the page is still on
/// `page_origin`). `mode` is "load", "focused" or "page" (see passwords-fill.js). The password
/// goes straight into the script's arguments: it isn't put anywhere the page could read before
/// the script runs, and the script only puts it in the sign-in form's password field.
fn fill(webview: &WebView, page_origin: &str, login: &Login, mode: &str) {
    // The page's data goes in last and in one piece, so nothing in it is taken for a
    // placeholder. The script checks the page is still on `page_origin`, the origin the login
    // was matched against (see `relation`).
    let arguments = json!([page_origin, login.username, login.password, mode]).to_string();
    let script = include_str!("passwords-fill.js")
        .replace("__DETECT__", DETECT)
        .replace("__ARGUMENTS__", &arguments);
    webview.evaluate_javascript(script, |_: Result<JSValue, _>| {});
}

/// Put a username or password on the clipboard, kept out of Windows' clipboard history and
/// cloud clipboard.
fn copy_secret(text: &str) {
    crate::desktop::clipboard::set_secret(text, "passwords");
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
