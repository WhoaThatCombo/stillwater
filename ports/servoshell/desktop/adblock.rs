/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Stillwater's built-in content blocker: uBlock Origin's default filter lists, matched with
//! Brave's adblock-rust engine.
//!
//! - **Network blocking** runs on Servo's network threads through `servo::set_request_filter`,
//!   so blocked requests never reach the main thread (and allowed ones skip that hop too).
//! - **Scriptlets** (`##+js(...)`, how uBlock Origin defuses ads that are built by page scripts)
//!   and site-specific **element hiding** go into a user script for the page's host, registered
//!   when the main page is requested so it runs before the page's own scripts.
//! - **Generic element hiding**: a small collector in every page reports the classes and ids it
//!   sees (via a tagged console message), and matching hide rules are injected as a stylesheet.
//!
//! Lists are downloaded on first use and refreshed every few days. The compiled engine is cached
//! on disk so later launches load it in milliseconds instead of re-parsing ~6 MB of lists.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::process::Command;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, RwLock};
use std::time::{Duration, SystemTime};

use adblock::Engine;
use adblock::lists::{FilterSet, ParseOptions};
use adblock::request::Request;
use adblock::resources::{PermissionMask, Resource};
use content_security_policy::Destination;
use log::warn;
use servo::{
    JSValue, RequestDecision, UserContentManager, UserScript, WebResourceRequest, WebView,
    WebViewId,
};
use url::Url;

use crate::desktop::diagnostics;

/// uBlock Origin's default lists ("uBlock filters" + EasyList, EasyPrivacy, Peter Lowe's), and
/// whether they're uBlock Origin's own (trusted to use privileged scriptlets).
const LISTS: &[(&str, &str, bool)] = &[
    ("ublock-filters", "https://ublockorigin.github.io/uAssets/filters/filters.min.txt", true),
    ("ublock-badware", "https://ublockorigin.github.io/uAssets/filters/badware.min.txt", true),
    ("ublock-privacy", "https://ublockorigin.github.io/uAssets/filters/privacy.min.txt", true),
    ("ublock-quick-fixes", "https://ublockorigin.github.io/uAssets/filters/quick-fixes.min.txt", true),
    ("ublock-unbreak", "https://ublockorigin.github.io/uAssets/filters/unbreak.min.txt", true),
    ("easylist", "https://ublockorigin.github.io/uAssets/thirdparties/easylist.txt", false),
    ("easyprivacy", "https://ublockorigin.github.io/uAssets/thirdparties/easyprivacy.txt", false),
    (
        "peter-lowe",
        "https://pgl.yoyo.org/adservers/serverlist.php?hostformat=adblockplus&showintro=0&mimetype=plaintext",
        false,
    ),
];

/// uBlock Origin's scriptlets converted for adblock-rust (tools/adblock/build-scriptlets.mjs).
const SCRIPTLETS_URL: &str = "https://whoathatcombo.github.io/stillwater/adblock/scriptlets.json";

/// How often to download fresh lists.
const REFRESH_AFTER: Duration = Duration::from_secs(4 * 24 * 60 * 60);

/// Permission bit for uBlock Origin's own lists (matches the converter's `TRUSTED`).
const TRUSTED: PermissionMask = PermissionMask::from_bits(1);

/// Hosts whose scriptlet user scripts stay registered (most recent first).
const MAX_HOST_SCRIPTS: usize = 24;

/// Console messages from the page-side collector start with this.
pub(crate) const COSMETIC_MESSAGE_PREFIX: &str = "__stillwater_adblock__";

struct Shared {
    engine: RwLock<Option<Arc<Engine>>>,
    enabled: AtomicBool,
    /// Hosts the user turned blocking off for.
    allowlist: RwLock<HashSet<String>>,
    /// The top-level page of each tab, so subresource requests know which site they belong to.
    pages: Mutex<HashMap<WebViewId, Url>>,
    /// Requests blocked on each tab's current page.
    blocked: Mutex<HashMap<WebViewId, u32>>,
    blocked_total: AtomicU64,
}

static SHARED: LazyLock<Shared> = LazyLock::new(|| Shared {
    engine: RwLock::new(None),
    enabled: AtomicBool::new(true),
    allowlist: RwLock::new(HashSet::new()),
    pages: Mutex::new(HashMap::new()),
    blocked: Mutex::new(HashMap::new()),
    blocked_total: AtomicU64::new(0),
});

fn engine() -> Option<Arc<Engine>> {
    SHARED.engine.read().ok()?.clone()
}

/// Where lists and the compiled engine are kept (`STILLWATER_ADBLOCK_DIR` overrides it for tests).
fn data_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("STILLWATER_ADBLOCK_DIR") {
        return Some(PathBuf::from(dir));
    }
    dirs::data_local_dir().map(|dir| dir.join("Stillwater").join("adblock"))
}

fn config_path() -> Option<PathBuf> {
    crate::desktop::settings::config_dir().map(|dir| dir.join("stillwater-adblock.json"))
}

/// Main-thread state: the user scripts registered for scriptlets.
pub(crate) struct Adblock {
    user_content_manager: Rc<UserContentManager>,
    /// The page-side class/id collector, present while blocking is on.
    collector: Option<Rc<UserScript>>,
    /// Per-host scriptlet + hiding scripts, most recently used first.
    host_scripts: VecDeque<(String, Rc<UserScript>)>,
}

impl Adblock {
    /// Load settings, install the network filter and start loading the engine in the background.
    pub(crate) fn new(user_content_manager: Rc<UserContentManager>) -> Self {
        let saved = config_path()
            .and_then(|path| std::fs::read_to_string(path).ok())
            .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
            .unwrap_or_default();
        let enabled = saved["enabled"].as_bool().unwrap_or(true) &&
            std::env::var_os("STILLWATER_NO_ADBLOCK").is_none();
        SHARED.enabled.store(enabled, Ordering::Relaxed);
        if let Some(hosts) = saved["allowlist"].as_array() &&
            let Ok(mut allowlist) = SHARED.allowlist.write()
        {
            allowlist.extend(hosts.iter().filter_map(|host| host.as_str().map(str::to_owned)));
        }

        servo::set_request_filter(Some(Arc::new(filter_request)));
        std::thread::Builder::new()
            .name("AdblockLoader".into())
            .spawn(load_engine)
            .ok();

        let mut adblock = Self {
            user_content_manager,
            collector: None,
            host_scripts: VecDeque::new(),
        };
        adblock.sync_collector();
        adblock
    }

    pub(crate) fn enabled(&self) -> bool {
        SHARED.enabled.load(Ordering::Relaxed)
    }

    pub(crate) fn set_enabled(&mut self, enabled: bool) {
        SHARED.enabled.store(enabled, Ordering::Relaxed);
        self.sync_collector();
        if !enabled {
            for (_, script) in self.host_scripts.drain(..) {
                self.user_content_manager.remove_script(script);
            }
        }
        save();
    }

    /// Whether blocking is on for `url`'s site.
    pub(crate) fn enabled_for(&self, url: &Url) -> bool {
        self.enabled() && !is_allowlisted(url)
    }

    /// Turn blocking off (or back on) for one site.
    pub(crate) fn toggle_site(&mut self, url: &Url) {
        let Some(host) = site_host(url) else {
            return;
        };
        if let Ok(mut allowlist) = SHARED.allowlist.write() &&
            !allowlist.remove(&host)
        {
            allowlist.insert(host.clone());
        }
        // Drop the site's scriptlets so a reload runs without them.
        if let Some(index) = self.host_scripts.iter().position(|(other, _)| *other == host) &&
            let Some((_, script)) = self.host_scripts.remove(index)
        {
            self.user_content_manager.remove_script(script);
        }
        save();
    }

    /// Requests blocked on a tab's current page.
    pub(crate) fn blocked_count(&self, webview: WebViewId) -> u32 {
        SHARED
            .blocked
            .lock()
            .ok()
            .and_then(|blocked| blocked.get(&webview).copied())
            .unwrap_or(0)
    }

    pub(crate) fn ready(&self) -> bool {
        engine().is_some()
    }

    /// A tab is about to load a new page: remember it and prepare that site's scriptlets.
    /// Called from `load_web_resource` (main thread), before the page's document exists.
    pub(crate) fn on_main_frame_request(&mut self, webview: WebViewId, url: &Url) {
        if debug_log() {
            diagnostics::log(format!("adblock: page {url} ({webview:?})"));
        }
        // Only web pages count as a new site (a data: or about: document keeps the last one).
        if !matches!(url.scheme(), "http" | "https") {
            return;
        }
        if let Ok(mut pages) = SHARED.pages.lock() {
            pages.insert(webview, url.clone());
        }
        if let Ok(mut blocked) = SHARED.blocked.lock() {
            blocked.insert(webview, 0);
        }
        if !self.enabled_for(url) {
            return;
        }
        let (Some(engine), Some(host)) = (engine(), url.host_str().map(str::to_owned)) else {
            return;
        };
        if let Some(index) = self.host_scripts.iter().position(|(other, _)| *other == host) {
            // Already registered: mark it recently used.
            if let Some(entry) = self.host_scripts.remove(index) {
                self.host_scripts.push_front(entry);
            }
            return;
        }

        let resources = engine.url_cosmetic_resources(url.as_str());
        if debug_log() {
            diagnostics::log(format!(
                "adblock: {host}: {} hide selectors, {} procedural, {} bytes of scriptlets",
                resources.hide_selectors.len(),
                resources.procedural_actions.len(),
                resources.injected_script.len()
            ));
        }
        if resources.injected_script.is_empty() && resources.hide_selectors.is_empty() {
            return;
        }
        let css = hide_css(resources.hide_selectors.iter());
        let script = format!(
            "(function () {{\n\
             if (location.hostname !== {host}) return;\n\
             var scriptletGlobals = {{}};\n\
             {css_part}\n\
             {scriptlets}\n\
             }})();",
            host = serde_json::to_string(&host).unwrap_or_default(),
            css_part = if css.is_empty() {
                String::new()
            } else {
                format!(
                    "try {{ var s = document.createElement('style'); s.textContent = {}; \
                     (document.head || document.documentElement).appendChild(s); }} catch (e) {{}}",
                    serde_json::to_string(&css).unwrap_or_default()
                )
            },
            scriptlets = resources.injected_script,
        );
        let script = Rc::new(UserScript::new(script, None));
        self.user_content_manager.add_script(script.clone());
        self.host_scripts.push_front((host, script));
        while self.host_scripts.len() > MAX_HOST_SCRIPTS {
            if let Some((_, old)) = self.host_scripts.pop_back() {
                self.user_content_manager.remove_script(old);
            }
        }
    }

    /// The page reported classes and ids (see `collector.js`): hide the ones generic rules target.
    pub(crate) fn on_cosmetic_message(&self, webview: &WebView, payload: &str) {
        let Some(url) = webview.url() else {
            return;
        };
        if !self.enabled_for(&url) {
            return;
        }
        let Some(engine) = engine() else {
            return;
        };
        let Ok((classes, ids)) = serde_json::from_str::<(Vec<String>, Vec<String>)>(payload) else {
            return;
        };
        let resources = engine.url_cosmetic_resources(url.as_str());
        if resources.generichide {
            return;
        }
        let selectors = engine.hidden_class_id_selectors(&classes, &ids, &resources.exceptions);
        if selectors.is_empty() {
            return;
        }
        let css = hide_css(selectors.iter());
        webview.evaluate_javascript(
            format!(
                "(function () {{ var s = document.getElementById('__stillwater_hide'); \
                 if (!s) {{ s = document.createElement('style'); s.id = '__stillwater_hide'; \
                 (document.head || document.documentElement).appendChild(s); }} \
                 s.textContent += {}; }})()",
                serde_json::to_string(&css).unwrap_or_default()
            ),
            |_: Result<JSValue, _>| {},
        );
    }

    /// Add or remove the page-side collector to match the on/off setting.
    fn sync_collector(&mut self) {
        match (self.enabled(), &self.collector) {
            (true, None) => {
                let script = Rc::new(UserScript::new(
                    include_str!("adblock-collector.js")
                        .replace("__PREFIX__", COSMETIC_MESSAGE_PREFIX),
                    None,
                ));
                self.user_content_manager.add_script(script.clone());
                self.collector = Some(script);
            },
            (false, Some(_)) => {
                if let Some(script) = self.collector.take() {
                    self.user_content_manager.remove_script(script);
                }
            },
            _ => {},
        }
    }
}

/// `selector { display: none !important; }` for each selector. One rule each, so a selector
/// the engine can't parse (e.g. a newer pseudo-class) only drops its own rule.
fn hide_css<'a>(selectors: impl Iterator<Item = &'a String>) -> String {
    selectors
        .map(|selector| format!("{selector} {{ display: none !important; }}\n"))
        .collect()
}

/// The host used for the per-site allowlist (without a leading `www.`).
fn site_host(url: &Url) -> Option<String> {
    let host = url.host_str()?;
    Some(host.strip_prefix("www.").unwrap_or(host).to_owned())
}

fn is_allowlisted(url: &Url) -> bool {
    let Some(host) = site_host(url) else {
        return false;
    };
    SHARED
        .allowlist
        .read()
        .is_ok_and(|allowlist| allowlist.contains(&host))
}

fn save() {
    let Some(path) = config_path() else {
        return;
    };
    let mut allowlist: Vec<String> = SHARED
        .allowlist
        .read()
        .map(|allowlist| allowlist.iter().cloned().collect())
        .unwrap_or_default();
    allowlist.sort();
    let data = serde_json::json!({
        "enabled": SHARED.enabled.load(Ordering::Relaxed),
        "allowlist": allowlist,
    });
    let result = path
        .parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .and_then(|_| std::fs::write(&path, data.to_string()));
    if let Err(error) = result {
        warn!("Could not save adblock settings to {}: {error}", path.display());
    }
}

/// Decide about one request. Runs on Servo's network threads, so it must stay fast.
fn filter_request(request: &WebResourceRequest, webview: Option<WebViewId>) -> RequestDecision {
    // Main pages go to the main thread, which prepares the site's scriptlets.
    if request.destination == Destination::Document {
        return RequestDecision::AskEmbedder;
    }
    if !SHARED.enabled.load(Ordering::Relaxed) {
        return RequestDecision::Allow;
    }
    let url = request.url.as_url();
    let request_type = match url.scheme() {
        "http" | "https" => request_type(request.destination),
        "ws" | "wss" => "websocket",
        _ => return RequestDecision::Allow,
    };
    let page = webview.and_then(|id| SHARED.pages.lock().ok()?.get(&id).cloned());
    if page.as_ref().is_some_and(is_allowlisted) {
        return RequestDecision::Allow;
    }
    let Some(engine) = engine() else {
        return RequestDecision::Allow;
    };
    let source = page
        .as_ref()
        .map(Url::as_str)
        .or_else(|| request.referrer_url.as_ref().map(|referrer| referrer.as_str()))
        .unwrap_or_else(|| url.as_str());
    let Ok(adblock_request) =
        Request::new(url.as_str(), source, request_type, request.method.as_str())
    else {
        return RequestDecision::Allow;
    };
    let result = engine.check_network_request(&adblock_request);
    if !result.should_block() {
        return RequestDecision::Allow;
    }
    // $redirect rules: serve uBlock Origin's harmless stand-in (e.g. a no-op ad SDK, which
    // keeps video players working) instead of failing the request.
    let decision = result
        .redirect
        .as_deref()
        .and_then(decode_data_url)
        .map(|(content_type, body)| RequestDecision::Respond { content_type, body })
        .unwrap_or(RequestDecision::Block);
    if let Some(webview) = webview &&
        let Ok(mut blocked) = SHARED.blocked.lock()
    {
        *blocked.entry(webview).or_default() += 1;
    }
    SHARED.blocked_total.fetch_add(1, Ordering::Relaxed);
    if debug_log() {
        let how = if matches!(decision, RequestDecision::Block) { "blocked" } else { "replaced" };
        diagnostics::log(format!("adblock: {how} {request_type} {url} ({webview:?})"));
    }
    decision
}

/// `STILLWATER_ADBLOCK_LOG`: log every blocked request and page start to the diagnostic log.
fn debug_log() -> bool {
    static ON: LazyLock<bool> = LazyLock::new(|| std::env::var_os("STILLWATER_ADBLOCK_LOG").is_some());
    *ON
}

/// Split a data:<mime>;base64,<data> URL into its type and bytes.
fn decode_data_url(url: &str) -> Option<(String, Vec<u8>)> {
    use base64::Engine as _;
    let (header, data) = url.strip_prefix("data:")?.split_once(',')?;
    let content_type = header.strip_suffix(";base64")?;
    let body = base64::engine::general_purpose::STANDARD.decode(data).ok()?;
    Some((content_type.to_owned(), body))
}

/// adblock-rust's name for what a request is for.
fn request_type(destination: Destination) -> &'static str {
    match destination {
        Destination::Script |
        Destination::Worker |
        Destination::SharedWorker |
        Destination::ServiceWorker |
        Destination::AudioWorklet |
        Destination::PaintWorklet |
        Destination::Xslt => "script",
        Destination::Style => "stylesheet",
        Destination::Image => "image",
        Destination::Font => "font",
        Destination::Audio | Destination::Video | Destination::Track => "media",
        Destination::IFrame | Destination::Frame => "subdocument",
        Destination::Object | Destination::Embed => "object",
        Destination::Report => "ping",
        // fetch() and XMLHttpRequest have no destination.
        Destination::None | Destination::Json => "xmlhttprequest",
        Destination::Document => "document",
        _ => "other",
    }
}

/// Background thread: load the cached engine, then refresh the lists if they're stale.
fn load_engine() {
    let Some(dir) = data_dir() else {
        return;
    };
    let _ = std::fs::create_dir_all(&dir);
    let cache = dir.join(format!("engine-{}.dat", ENGINE_FORMAT));
    let scriptlets = dir.join("scriptlets.json");

    let started = std::time::Instant::now();
    if let Ok(bytes) = std::fs::read(&cache) {
        let mut engine = Engine::default();
        match engine.deserialize(&bytes) {
            Ok(()) => {
                use_scriptlets(&mut engine, &scriptlets);
                publish(engine);
                diagnostics::log(format!("adblock: cached engine loaded in {:?}", started.elapsed()));
            },
            Err(error) => diagnostics::log(format!("adblock: cached engine unusable: {error:?}")),
        }
    }

    let stale = std::fs::metadata(&cache)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .is_none_or(|age| age > REFRESH_AFTER);
    if !stale && engine().is_some() {
        return;
    }

    // Download the lists (keeping the previous copy of any that fail) and rebuild.
    let started = std::time::Instant::now();
    let mut filter_set = FilterSet::new(false);
    let mut loaded = 0;
    for (name, url, trusted) in LISTS {
        let path = dir.join(format!("{name}.txt"));
        if let Err(error) = download(url, &path) {
            diagnostics::log(format!("adblock: couldn't download {name}: {error}"));
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let options = ParseOptions {
            permissions: if *trusted { TRUSTED } else { PermissionMask::default() },
            ..ParseOptions::default()
        };
        filter_set.add_filter_list(text, options);
        loaded += 1;
    }
    // STILLWATER_ADBLOCK_SCRIPTLETS: use a local scriptlets.json (testing a new conversion).
    let fetched = match std::env::var_os("STILLWATER_ADBLOCK_SCRIPTLETS") {
        Some(local) => std::fs::copy(local, &scriptlets)
            .map(|_| ())
            .map_err(|error| error.to_string()),
        None => download(SCRIPTLETS_URL, &scriptlets),
    };
    if let Err(error) = fetched {
        diagnostics::log(format!("adblock: couldn't download scriptlets: {error}"));
    }
    if loaded == 0 {
        diagnostics::log("adblock: no filter lists available");
        return;
    }
    let mut engine = Engine::new_with_filter_set(filter_set);
    if let Err(error) = std::fs::write(&cache, engine.serialize()) {
        diagnostics::log(format!("adblock: couldn't cache engine: {error}"));
    }
    use_scriptlets(&mut engine, &scriptlets);
    publish(engine);
    diagnostics::log(format!(
        "adblock: {loaded} lists compiled in {:?}",
        started.elapsed()
    ));
}

/// Bump when the cached engine format changes (it's tied to the adblock crate version).
const ENGINE_FORMAT: &str = "0.13";

fn use_scriptlets(engine: &mut Engine, path: &std::path::Path) {
    let resources = std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<Vec<Resource>>(&text).ok());
    match resources {
        Some(resources) => engine.use_resources(resources),
        None => diagnostics::log("adblock: no scriptlets yet"),
    }
}

fn publish(engine: Engine) {
    if let Ok(mut slot) = SHARED.engine.write() {
        *slot = Some(Arc::new(engine));
    }
}

/// Download `url` to `path` (via a temporary file, so a failure keeps the old copy).
fn download(url: &str, path: &std::path::Path) -> Result<(), String> {
    let partial = path.with_extension("part");
    let mut command = Command::new("curl.exe");
    command.args([
        "--silent",
        "--show-error",
        "--fail",
        "--location",
        "--max-time",
        "120",
        "--user-agent",
        "Stillwater-Adblock",
        "--output",
    ]);
    command.arg(&partial).arg(url);
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let output = command
        .output()
        .map_err(|error| format!("couldn't run curl.exe: {error}"))?;
    if !output.status.success() {
        let _ = std::fs::remove_file(&partial);
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_owned());
    }
    std::fs::rename(&partial, path).map_err(|error| error.to_string())
}
