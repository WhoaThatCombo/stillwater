/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Page translation, on this computer, like Firefox's and Zen's.
//!
//! When a page finishes loading, its language is worked out from the page's text (and its
//! `lang` attribute). A page in another language is translated automatically (unless the user
//! turned that off, or said never for the language or site), else the address bar offers it.
//!
//! The models are Firefox Translations' (published by Mozilla, the same ones Firefox and Zen
//! use), downloaded once per language pair (about 20 to 40 MB) into the profile and checked
//! against Mozilla's SHA-256. They're run by `stillwater_translate`, Stillwater's own engine in
//! Rust; nothing is sent anywhere. Languages without a direct model go through English.
//!
//! In the page, `translate-page.js` hands out text a block at a time, visible text first, and
//! puts the translations back, keeping links and emphasis in place. New text the page adds later
//! is picked up by polling while a page is translated.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use log::warn;
use serde_json::{Value, json};
use servo::{JSValue, LoadStatus, WebView, WebViewId};
use sha2::{Digest, Sha256};
use stillwater_translate::Translator;
use winit::event_loop::EventLoopProxy;

use crate::desktop::diagnostics;
use crate::desktop::event_loop::AppEvent;
use crate::desktop::settings::config_dir;

const PAGE_JS: &str = include_str!("translate-page.js");
const CATALOG_URL: &str = "https://firefox.settings.services.mozilla.com/v1/buckets/main/collections/translations-models/records";
const CDN: &str = "https://firefox-settings-attachments.cdn.mozilla.net/";
/// How often a translated page is checked for new text.
const POLL: Duration = Duration::from_millis(1500);
/// Language pairs kept loaded at once.
const LOADED_PAIRS: usize = 4;

/// Reads the page's declared language and a sample of its text.
const DETECT_JS: &str = r#"(() => {
  const root = document.documentElement;
  const meta = document.querySelector('meta[http-equiv="content-language" i]');
  const main = document.querySelector("main, article, [role=main]") || document.body;
  const text = main ? (main.innerText || "").slice(0, 4000) : "";
  return JSON.stringify({ lang: (root && root.lang) || (meta && meta.content) || "", text });
})()"#;

/// Firefox Translations' languages and their names.
const LANGUAGES: &[(&str, &str)] = &[
    ("af", "Afrikaans"),
    ("ar", "Arabic"),
    ("az", "Azerbaijani"),
    ("be", "Belarusian"),
    ("bg", "Bulgarian"),
    ("bn", "Bengali"),
    ("bs", "Bosnian"),
    ("ca", "Catalan"),
    ("cs", "Czech"),
    ("da", "Danish"),
    ("de", "German"),
    ("el", "Greek"),
    ("en", "English"),
    ("es", "Spanish"),
    ("et", "Estonian"),
    ("eu", "Basque"),
    ("fa", "Persian"),
    ("fi", "Finnish"),
    ("fr", "French"),
    ("gl", "Galician"),
    ("gu", "Gujarati"),
    ("he", "Hebrew"),
    ("hi", "Hindi"),
    ("hr", "Croatian"),
    ("hu", "Hungarian"),
    ("id", "Indonesian"),
    ("is", "Icelandic"),
    ("it", "Italian"),
    ("ja", "Japanese"),
    ("kn", "Kannada"),
    ("ko", "Korean"),
    ("lt", "Lithuanian"),
    ("lv", "Latvian"),
    ("ml", "Malayalam"),
    ("mr", "Marathi"),
    ("ms", "Malay"),
    ("mt", "Maltese"),
    ("nb", "Norwegian Bokmål"),
    ("nl", "Dutch"),
    ("nn", "Norwegian Nynorsk"),
    ("pl", "Polish"),
    ("pt", "Portuguese"),
    ("ro", "Romanian"),
    ("ru", "Russian"),
    ("sk", "Slovak"),
    ("sl", "Slovenian"),
    ("sq", "Albanian"),
    ("sr", "Serbian"),
    ("sv", "Swedish"),
    ("ta", "Tamil"),
    ("te", "Telugu"),
    ("th", "Thai"),
    ("tl", "Tagalog"),
    ("tr", "Turkish"),
    ("ug", "Uyghur"),
    ("uk", "Ukrainian"),
    ("ur", "Urdu"),
    ("vi", "Vietnamese"),
    ("zh-Hans", "Chinese (Simplified)"),
    ("zh-Hant", "Chinese (Traditional)"),
];

pub(crate) fn language_name(code: &str) -> String {
    LANGUAGES
        .iter()
        .find(|(id, _)| *id == code)
        .map(|(_, name)| (*name).to_owned())
        .unwrap_or_else(|| code.to_owned())
}

/// A declared language tag (`de-DE`, `zh-TW`, `iw`) as one of Firefox Translations' codes.
fn normalize_tag(tag: &str) -> Option<String> {
    let tag = tag.trim().to_ascii_lowercase().replace('_', "-");
    let primary = tag.split('-').next().unwrap_or("");
    let code = match primary {
        "" => return None,
        "zh" if ["tw", "hk", "mo", "hant"]
            .iter()
            .any(|part| tag.split('-').any(|piece| piece == *part)) =>
        {
            "zh-Hant"
        },
        "zh" => "zh-Hans",
        "no" => "nb",
        "iw" => "he",
        "in" => "id",
        "fil" => "tl",
        "pt" | "en" | "es" | "fr" | "de" => primary,
        other => other,
    };
    LANGUAGES
        .iter()
        .any(|(id, _)| *id == code)
        .then(|| code.to_owned())
}

/// The language of a page: what its text looks like, or what it says it is when the text is
/// too short or ambiguous to tell.
fn detect(declared: &str, text: &str) -> Option<String> {
    let declared = normalize_tag(declared);
    let detected = whatlang::detect(text)
        .filter(|info| {
            info.is_reliable() && text.chars().filter(|c| c.is_alphabetic()).count() >= 40
        })
        .and_then(|info| {
            let code = match info.lang().code() {
                "eng" => "en",
                "deu" => "de",
                "fra" => "fr",
                "spa" => "es",
                "ita" => "it",
                "por" => "pt",
                "nld" => "nl",
                "pol" => "pl",
                "rus" => "ru",
                "ukr" => "uk",
                "jpn" => "ja",
                "kor" => "ko",
                "ara" => "ar",
                "tur" => "tr",
                "ces" => "cs",
                "slk" => "sk",
                "slv" => "sl",
                "hrv" => "hr",
                "srp" => "sr",
                "bul" => "bg",
                "ron" => "ro",
                "hun" => "hu",
                "fin" => "fi",
                "swe" => "sv",
                "dan" => "da",
                "nob" => "nb",
                "est" => "et",
                "lav" => "lv",
                "lit" => "lt",
                "ell" => "el",
                "heb" => "he",
                "hin" => "hi",
                "ben" => "bn",
                "guj" => "gu",
                "mar" => "mr",
                "tam" => "ta",
                "tel" => "te",
                "kan" => "kn",
                "mal" => "ml",
                "urd" => "ur",
                "pes" => "fa",
                "tha" => "th",
                "vie" => "vi",
                "ind" => "id",
                "tgl" => "tl",
                "cat" => "ca",
                "aze" => "az",
                "bel" => "be",
                "afr" => "af",
                "epo" => return None,
                "cmn" => {
                    // Simplified or Traditional: what the page says, else Simplified.
                    return Some(
                        declared
                            .clone()
                            .filter(|code| code.starts_with("zh"))
                            .unwrap_or_else(|| "zh-Hans".to_owned()),
                    );
                },
                _ => return None,
            };
            Some(code.to_owned())
        });
    detected.or(declared)
}

/// The user's Windows display language, as a translation target.
fn system_language() -> String {
    #[cfg(target_os = "windows")]
    {
        use windows_sys::Win32::Globalization::GetUserDefaultLocaleName;
        let mut buffer = [0u16; 85];
        // Safety: the buffer is the size the call is told.
        let length = unsafe { GetUserDefaultLocaleName(buffer.as_mut_ptr(), buffer.len() as i32) };
        if length > 1 {
            let tag = String::from_utf16_lossy(&buffer[..length as usize - 1]);
            if let Some(code) = normalize_tag(&tag) {
                return code;
            }
        }
    }
    "en".to_owned()
}

/// One file of a language pair in Mozilla's catalog.
#[derive(Clone, Debug)]
pub(crate) struct ModelFile {
    name: String,
    hash: String,
    location: String,
}

/// Mozilla's catalog of models: for each pair (`de-en`), the newest stable version's files.
pub(crate) type Catalog = HashMap<String, (String, Vec<ModelFile>)>;

/// Whether a record's filter (Remote Settings' JEXL, in the simple forms Mozilla uses for
/// models) picks it for release Firefox on desktop, which is what Stillwater takes after:
/// `env.channel == 'nightly' || env.appinfo.OS != 'Android'` and the like. Anything else is
/// left out.
fn for_desktop_release(filter: &str) -> bool {
    let filter = filter.trim();
    if filter.is_empty() {
        return true;
    }
    let term = |term: &str| -> Option<bool> {
        let (key, value, equal) = if let Some((key, value)) = term.split_once("!=") {
            (key, value, false)
        } else {
            let (key, value) = term.split_once("==")?;
            (key, value, true)
        };
        let value = value.trim().trim_matches(['\'', '"']);
        let actual = match key.trim() {
            "env.channel" => "release",
            "env.appinfo.OS" => "WINNT",
            _ => return None,
        };
        Some((actual == value) == equal)
    };
    filter.split("||").any(|alternative| {
        alternative
            .split("&&")
            .all(|part| term(part).unwrap_or(false))
    })
}

fn parse_catalog(text: &str) -> Result<Catalog, String> {
    let value: Value = serde_json::from_str(text).map_err(|error| error.to_string())?;
    let records = value["data"]
        .as_array()
        .ok_or("The model list has no records.")?;
    let version_key = |version: &str| -> Option<Vec<u64>> {
        version.split('.').map(|part| part.parse().ok()).collect()
    };
    let mut catalog: Catalog = HashMap::new();
    for record in records {
        let (Some(from), Some(to), Some(version), Some(name)) = (
            record["fromLang"].as_str(),
            record["toLang"].as_str(),
            record["version"].as_str(),
            record["name"].as_str(),
        ) else {
            continue;
        };
        // Release models only: no alphas, nothing restricted to Nightly.
        let Some(key) = version_key(version) else {
            continue;
        };
        if !for_desktop_release(record["filter_expression"].as_str().unwrap_or("")) {
            continue;
        }
        let attachment = &record["attachment"];
        let file = ModelFile {
            name: name.to_owned(),
            hash: attachment["hash"].as_str().unwrap_or("").to_owned(),
            location: attachment["location"].as_str().unwrap_or("").to_owned(),
        };
        if file.hash.is_empty() || file.location.is_empty() || file.name.contains(['/', '\\']) {
            continue;
        }
        let pair = format!("{from}-{to}");
        let entry = catalog
            .entry(pair)
            .or_insert_with(|| (version.to_owned(), Vec::new()));
        let current = version_key(&entry.0).unwrap_or_default();
        if key > current {
            *entry = (version.to_owned(), vec![file]);
        } else if key == current {
            entry.1.push(file);
        }
    }
    // A pair needs its model and vocabulary.
    catalog.retain(|_, (_, files)| {
        files.iter().any(|file| file.name.starts_with("model."))
            && files.iter().any(|file| file.name.contains("vocab."))
    });
    Ok(catalog)
}

fn translations_dir() -> Option<PathBuf> {
    config_dir().map(|dir| dir.join("translations"))
}

fn model_dir(pair: &str) -> Option<PathBuf> {
    translations_dir().map(|dir| dir.join("models").join(pair))
}

/// Whether a pair's files are on disk.
fn installed(pair: &str) -> bool {
    model_dir(pair).is_some_and(|dir| dir.join("ready").exists())
}

fn curl() -> Command {
    let mut command = Command::new("curl.exe");
    command.args([
        "--silent",
        "--show-error",
        "--fail",
        "--location",
        "--max-time",
        "600",
        "--user-agent",
        "Stillwater",
    ]);
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
}

/// Mozilla's model list, from the profile when it's under a week old.
fn fetch_catalog() -> Result<Catalog, String> {
    let cache = translations_dir().map(|dir| dir.join("catalog.json"));
    if let Some(cache) = &cache {
        let fresh = std::fs::metadata(cache)
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|modified| SystemTime::now().duration_since(modified).ok())
            .is_some_and(|age| age < Duration::from_secs(7 * 24 * 3600));
        if fresh {
            if let Ok(catalog) = std::fs::read_to_string(cache)
                .map_err(|e| e.to_string())
                .and_then(|text| parse_catalog(&text))
            {
                return Ok(catalog);
            }
        }
    }
    let output = curl()
        .arg(CATALOG_URL)
        .output()
        .map_err(|error| format!("Couldn't run curl: {error}"))?;
    if !output.status.success() {
        // Offline: an old copy is better than nothing.
        if let Some(Ok(catalog)) = cache.as_ref().map(|cache| {
            std::fs::read_to_string(cache)
                .map_err(|e| e.to_string())
                .and_then(|text| parse_catalog(&text))
        }) {
            return Ok(catalog);
        }
        return Err("Couldn't get the list of translation models. Are you online?".into());
    }
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    let catalog = parse_catalog(&text)?;
    if let Some(cache) = &cache {
        let _ = cache.parent().map(std::fs::create_dir_all);
        let _ = std::fs::write(cache, &text);
    }
    Ok(catalog)
}

fn sha256_hex(path: &Path) -> Result<String, String> {
    let bytes = std::fs::read(path).map_err(|error| error.to_string())?;
    Ok(Sha256::digest(&bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

/// Downloads a pair's files, each checked against Mozilla's SHA-256.
fn download_pair(pair: &str, version: &str, files: &[ModelFile]) -> Result<(), String> {
    let dir = model_dir(pair).ok_or("No profile folder to save the model in.")?;
    let partial = dir.with_extension("partial");
    let _ = std::fs::remove_dir_all(&partial);
    std::fs::create_dir_all(&partial).map_err(|error| error.to_string())?;
    for file in files {
        let target = partial.join(&file.name);
        let status = curl()
            .arg("--output")
            .arg(&target)
            .arg(format!("{CDN}{}", file.location))
            .status()
            .map_err(|error| format!("Couldn't run curl: {error}"))?;
        if !status.success() {
            return Err(format!("Couldn't download {}.", file.name));
        }
        if sha256_hex(&target)? != file.hash.to_ascii_lowercase() {
            let _ = std::fs::remove_dir_all(&partial);
            return Err(format!("{} didn't match Mozilla's checksum.", file.name));
        }
    }
    std::fs::write(partial.join("ready"), version).map_err(|error| error.to_string())?;
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::rename(&partial, &dir).map_err(|error| error.to_string())?;
    Ok(())
}

/// Events from translation work done elsewhere (page scripts, worker threads).
pub(crate) enum TranslateEvent {
    Detected {
        webview: WebViewId,
        generation: u64,
        declared: String,
        text: String,
    },
    Collected {
        webview: WebViewId,
        generation: u64,
        blocks: Vec<(u64, Vec<String>)>,
    },
    Translated {
        webview: WebViewId,
        generation: u64,
        results: Vec<(u64, Vec<String>)>,
    },
    Failed {
        webview: WebViewId,
        generation: u64,
        error: String,
    },
    Poll {
        webview: WebViewId,
        generation: u64,
    },
    Catalog(Result<Catalog, String>),
    Downloaded {
        pair: String,
        result: Result<(), String>,
    },
}

// The diagnostic log gets what happened, never the page's text.
impl std::fmt::Debug for TranslateEvent {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TranslateEvent::Detected { declared, .. } => {
                write!(formatter, "Detected(declared {declared:?})")
            },
            TranslateEvent::Collected { blocks, .. } => {
                write!(formatter, "Collected({} blocks)", blocks.len())
            },
            TranslateEvent::Translated { results, .. } => {
                write!(formatter, "Translated({} blocks)", results.len())
            },
            TranslateEvent::Failed { error, .. } => write!(formatter, "Failed({error})"),
            TranslateEvent::Poll { .. } => write!(formatter, "Poll"),
            TranslateEvent::Catalog(result) => match result {
                Ok(catalog) => write!(formatter, "Catalog({} pairs)", catalog.len()),
                Err(error) => write!(formatter, "Catalog(failed: {error})"),
            },
            TranslateEvent::Downloaded { pair, result } => {
                write!(formatter, "Downloaded({pair}, {result:?})")
            },
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
enum Status {
    /// Loading, or not a web page.
    Idle,
    Detecting,
    /// Already in the language pages are translated into.
    Same,
    /// In another language, not translated (the address bar offers it).
    Offer,
    /// Waiting for models to download.
    Downloading,
    Translating,
    Translated,
    /// No models for this language.
    Unsupported,
    Failed(String),
}

struct Page {
    generation: u64,
    host: String,
    from: Option<String>,
    status: Status,
    /// A batch is out (being collected, translated or applied).
    busy: bool,
    /// Language checks so far (a page may have no text yet when it finishes loading).
    tries: u8,
}

struct Prefs {
    auto: bool,
    target: String,
    always: Vec<String>,
    never_languages: Vec<String>,
    never_sites: Vec<String>,
}

type Engines = Arc<Mutex<Vec<(String, Arc<Translator>)>>>;

pub(crate) struct Translations {
    prefs: Prefs,
    pages: HashMap<WebViewId, Page>,
    catalog: Option<Catalog>,
    catalog_status: Option<String>,
    fetching_catalog: bool,
    downloading: HashSet<String>,
    engines: Engines,
    proxy: Option<EventLoopProxy<AppEvent>>,
    next_generation: u64,
}

impl Translations {
    pub(crate) fn new() -> Self {
        let saved = translations_dir()
            .and_then(|dir| std::fs::read_to_string(dir.join("settings.json")).ok())
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .unwrap_or(Value::Null);
        let list = |key: &str| -> Vec<String> {
            saved[key]
                .as_array()
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| item.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default()
        };
        let target = saved["target"]
            .as_str()
            .and_then(normalize_tag)
            .unwrap_or_else(system_language);
        Self {
            prefs: Prefs {
                auto: saved["auto"].as_bool().unwrap_or(true),
                target,
                always: list("always"),
                never_languages: list("neverLanguages"),
                never_sites: list("neverSites"),
            },
            pages: HashMap::new(),
            catalog: None,
            catalog_status: None,
            fetching_catalog: false,
            downloading: HashSet::new(),
            engines: Arc::new(Mutex::new(Vec::new())),
            proxy: None,
            next_generation: 1,
        }
    }

    fn save(&self) {
        let Some(dir) = translations_dir() else {
            return;
        };
        let data = json!({
            "auto": self.prefs.auto,
            "target": self.prefs.target,
            "always": self.prefs.always,
            "neverLanguages": self.prefs.never_languages,
            "neverSites": self.prefs.never_sites,
        });
        let result = std::fs::create_dir_all(&dir)
            .and_then(|_| std::fs::write(dir.join("settings.json"), data.to_string()));
        if let Err(error) = result {
            warn!("Couldn't save translation settings: {error}");
        }
    }

    pub(crate) fn set_proxy(&mut self, proxy: &EventLoopProxy<AppEvent>) {
        if self.proxy.is_none() {
            self.proxy = Some(proxy.clone());
        }
    }

    fn sender(&self) -> Option<impl Fn(TranslateEvent) + Send + Clone + 'static> {
        let proxy = self.proxy.clone()?;
        Some(move |event: TranslateEvent| {
            let _ = proxy.send_event(AppEvent::Translate(event));
        })
    }

    /// Ask the page what language it's in.
    fn detect_page(&mut self, webview: &WebView) {
        let Some(send) = self.sender() else {
            diagnostics::log("translate: no event loop yet".to_owned());
            return;
        };
        let Some(generation) = self.pages.get(&webview.id()).map(|page| page.generation) else {
            return;
        };
        let id = webview.id();
        webview.evaluate_javascript(DETECT_JS.to_owned(), move |result| {
            let value = match result {
                Ok(JSValue::String(json)) => {
                    serde_json::from_str::<Value>(&json).unwrap_or(Value::Null)
                },
                _ => Value::Null,
            };
            send(TranslateEvent::Detected {
                webview: id,
                generation,
                declared: value["lang"].as_str().unwrap_or("").to_owned(),
                text: value["text"].as_str().unwrap_or("").to_owned(),
            });
        });
    }

    /// A tab started or finished loading a page.
    pub(crate) fn on_load(&mut self, webview: &WebView, status: LoadStatus) {
        match status {
            LoadStatus::Started => {
                let generation = self.next_generation;
                self.next_generation += 1;
                self.pages.insert(
                    webview.id(),
                    Page {
                        generation,
                        host: String::new(),
                        from: None,
                        status: Status::Idle,
                        busy: false,
                        tries: 0,
                    },
                );
            },
            LoadStatus::Complete => {
                let Some(url) = webview
                    .url()
                    .filter(|url| matches!(url.scheme(), "http" | "https"))
                else {
                    return;
                };
                // Some pages report finishing more than once; a new page always starts first.
                if self
                    .pages
                    .get(&webview.id())
                    .is_some_and(|page| page.status != Status::Idle)
                {
                    return;
                }
                let generation = self.next_generation;
                self.next_generation += 1;
                self.pages.insert(
                    webview.id(),
                    Page {
                        generation,
                        host: url.host_str().unwrap_or("").to_owned(),
                        from: None,
                        status: Status::Detecting,
                        busy: false,
                        tries: 0,
                    },
                );
                self.detect_page(webview);
            },
            _ => {},
        }
    }

    /// The pairs to go from `from` to the target: direct, or through English.
    fn chain(&self, from: &str) -> Option<Vec<String>> {
        let to = &self.prefs.target;
        let available = |pair: &str| {
            installed(pair)
                || self
                    .catalog
                    .as_ref()
                    .is_some_and(|catalog| catalog.contains_key(pair))
        };
        let direct = format!("{from}-{to}");
        if available(&direct) {
            return Some(vec![direct]);
        }
        if from != "en" && to != "en" {
            let (first, second) = (format!("{from}-en"), format!("en-{to}"));
            if available(&first) && available(&second) {
                return Some(vec![first, second]);
            }
        }
        None
    }

    fn ensure_catalog(&mut self) {
        if self.catalog.is_some() || self.fetching_catalog {
            return;
        }
        let Some(send) = self.sender() else {
            return;
        };
        self.fetching_catalog = true;
        std::thread::Builder::new()
            .name("TranslationModels".into())
            .spawn(move || send(TranslateEvent::Catalog(fetch_catalog())))
            .ok();
    }

    /// Start translating a tab's page (models permitting).
    fn start(&mut self, webview: &WebView) {
        let Some(page) = self.pages.get(&webview.id()) else {
            return;
        };
        let Some(from) = page.from.clone() else {
            return;
        };
        let chain = self.chain(&from);
        let Some(chain) = chain else {
            if self.catalog.is_none() {
                // The catalog will say whether there's a model; try again when it's here.
                self.set_status(webview.id(), Status::Downloading);
                self.ensure_catalog();
            } else {
                self.set_status(webview.id(), Status::Unsupported);
            }
            return;
        };
        let missing: Vec<String> = chain
            .iter()
            .filter(|pair| !installed(pair))
            .cloned()
            .collect();
        if missing.is_empty() {
            self.set_status(webview.id(), Status::Translating);
            self.collect(webview);
            return;
        }
        self.set_status(webview.id(), Status::Downloading);
        if self.catalog.is_none() {
            self.ensure_catalog();
            return;
        }
        for pair in missing {
            self.download(pair);
        }
    }

    fn download(&mut self, pair: String) {
        if self.downloading.contains(&pair) {
            return;
        }
        let Some((version, files)) = self
            .catalog
            .as_ref()
            .and_then(|catalog| catalog.get(&pair))
            .cloned()
        else {
            return;
        };
        let Some(send) = self.sender() else {
            return;
        };
        self.downloading.insert(pair.clone());
        std::thread::Builder::new()
            .name("TranslationDownload".into())
            .spawn(move || {
                let result = download_pair(&pair, &version, &files);
                send(TranslateEvent::Downloaded { pair, result });
            })
            .ok();
    }

    fn set_status(&mut self, webview: WebViewId, status: Status) {
        diagnostics::log(format!("translate: {status:?}"));
        if let Some(page) = self.pages.get_mut(&webview) {
            page.status = status;
        }
    }

    /// Ask the page for its next batch of text.
    fn collect(&mut self, webview: &WebView) {
        let Some(send) = self.sender() else {
            return;
        };
        let Some(page) = self.pages.get_mut(&webview.id()) else {
            return;
        };
        if page.busy {
            return;
        }
        page.busy = true;
        let generation = page.generation;
        let id = webview.id();
        webview.evaluate_javascript(
            format!("{PAGE_JS}\nwindow.__stillwaterTranslate.collect({generation})"),
            move |result| {
                let blocks = match result {
                    Ok(JSValue::String(json)) => serde_json::from_str::<Value>(&json)
                        .ok()
                        .and_then(|value| value.as_array().cloned())
                        .unwrap_or_default()
                        .into_iter()
                        .filter_map(|block| {
                            let id = block["i"].as_u64()?;
                            let runs = block["r"]
                                .as_array()?
                                .iter()
                                .map(|run| run.as_str().unwrap_or("").to_owned())
                                .collect();
                            Some((id, runs))
                        })
                        .collect(),
                    Ok(_) => Vec::new(),
                    Err(error) => {
                        send(TranslateEvent::Failed {
                            webview: id,
                            generation,
                            error: format!("The page couldn't be read ({error:?})."),
                        });
                        return;
                    },
                };
                send(TranslateEvent::Collected {
                    webview: id,
                    generation,
                    blocks,
                });
            },
        );
    }

    /// Translate a batch on a worker thread.
    fn translate(&mut self, webview: WebViewId, generation: u64, blocks: Vec<(u64, Vec<String>)>) {
        let Some(from) = self.pages.get(&webview).and_then(|page| page.from.clone()) else {
            return;
        };
        let (Some(chain), Some(send)) = (self.chain(&from), self.sender()) else {
            return;
        };
        let engines = self.engines.clone();
        std::thread::Builder::new()
            .name("Translate".into())
            .spawn(move || {
                let started = std::time::Instant::now();
                let mut runs: Vec<Vec<String>> =
                    blocks.iter().map(|(_, runs)| runs.clone()).collect();
                for pair in &chain {
                    let engine = match engine(&engines, pair) {
                        Ok(engine) => engine,
                        Err(error) => {
                            send(TranslateEvent::Failed {
                                webview,
                                generation,
                                error,
                            });
                            return;
                        },
                    };
                    let borrowed: Vec<Vec<&str>> = runs
                        .iter()
                        .map(|block| block.iter().map(String::as_str).collect())
                        .collect();
                    runs = engine.translate_blocks(&borrowed);
                }
                diagnostics::log(format!(
                    "translate: {} blocks translated in {:.2}s",
                    blocks.len(),
                    started.elapsed().as_secs_f32()
                ));
                let results = blocks.into_iter().map(|(id, _)| id).zip(runs).collect();
                send(TranslateEvent::Translated {
                    webview,
                    generation,
                    results,
                });
            })
            .ok();
    }

    pub(crate) fn handle(
        &mut self,
        event: TranslateEvent,
        find: impl Fn(WebViewId) -> Option<WebView>,
    ) {
        let current = |pages: &HashMap<WebViewId, Page>, webview: WebViewId, generation: u64| {
            pages
                .get(&webview)
                .is_some_and(|page| page.generation == generation)
        };
        match event {
            TranslateEvent::Detected {
                webview,
                generation,
                declared,
                text,
            } => {
                if !current(&self.pages, webview, generation) {
                    return;
                }
                let from = detect(&declared, &text);
                diagnostics::log(format!(
                    "translate: page language {from:?} (declared \"{declared}\", {} characters of text)",
                    text.len()
                ));
                let target = self.prefs.target.clone();
                let page = self.pages.get_mut(&webview).unwrap();
                page.from = from.clone();
                let Some(from) = from else {
                    // No text yet (pages that build themselves with script): look again shortly.
                    if page.tries < 6 {
                        page.tries += 1;
                        self.poll_later(webview, generation);
                    } else {
                        page.status = Status::Same;
                    }
                    return;
                };
                if from == target {
                    page.status = Status::Same;
                    return;
                }
                page.status = Status::Offer;
                let host = page.host.clone();
                let wanted = (self.prefs.auto || self.prefs.always.contains(&from))
                    && !self.prefs.never_languages.contains(&from)
                    && !self.prefs.never_sites.contains(&host);
                if wanted {
                    if let Some(webview) = find(webview) {
                        self.start(&webview);
                    }
                } else {
                    // Know whether the offer can be honoured.
                    self.ensure_catalog();
                }
            },
            TranslateEvent::Collected {
                webview,
                generation,
                blocks,
            } => {
                if !current(&self.pages, webview, generation) {
                    return;
                }
                if !blocks.is_empty() {
                    diagnostics::log(format!("translate: {} blocks to translate", blocks.len()));
                }
                if blocks.is_empty() {
                    let page = self.pages.get_mut(&webview).unwrap();
                    page.busy = false;
                    if matches!(page.status, Status::Translating | Status::Translated) {
                        page.status = Status::Translated;
                        self.poll_later(webview, generation);
                    }
                    return;
                }
                self.translate(webview, generation, blocks);
            },
            TranslateEvent::Translated {
                webview,
                generation,
                results,
            } => {
                if !current(&self.pages, webview, generation) {
                    return;
                }
                let Some(view) = find(webview) else {
                    return;
                };
                let payload: Vec<Value> = results
                    .iter()
                    .map(|(id, runs)| json!({ "i": id, "r": runs }))
                    .collect();
                let Some(send) = self.sender() else {
                    return;
                };
                view.evaluate_javascript(
                    format!(
                        "window.__stillwaterTranslate && window.__stillwaterTranslate.apply({}, {})",
                        Value::Array(payload),
                        Value::String(self.prefs.target.clone()),
                    ),
                    move |_| send(TranslateEvent::Poll { webview, generation }),
                );
                if let Some(page) = self.pages.get_mut(&webview) {
                    page.busy = false;
                }
            },
            TranslateEvent::Poll {
                webview,
                generation,
            } => {
                if !current(&self.pages, webview, generation) {
                    return;
                }
                let status = self.pages.get(&webview).map(|page| page.status.clone());
                if status == Some(Status::Detecting) {
                    if let Some(view) = find(webview) {
                        self.detect_page(&view);
                    }
                    return;
                }
                let translating = matches!(status, Some(Status::Translating | Status::Translated));
                if translating {
                    if let Some(view) = find(webview) {
                        self.collect(&view);
                    }
                }
            },
            TranslateEvent::Failed {
                webview,
                generation,
                error,
            } => {
                if !current(&self.pages, webview, generation) {
                    return;
                }
                warn!("Translation failed: {error}");
                diagnostics::log(format!("translate: failed: {error}"));
                if let Some(page) = self.pages.get_mut(&webview) {
                    page.busy = false;
                    page.status = Status::Failed(error);
                }
            },
            TranslateEvent::Catalog(result) => {
                self.fetching_catalog = false;
                match result {
                    Ok(catalog) => {
                        self.catalog = Some(catalog);
                        self.catalog_status = None;
                    },
                    Err(error) => {
                        self.catalog_status = Some(error.clone());
                        for page in self.pages.values_mut() {
                            if page.status == Status::Downloading {
                                page.status = Status::Failed(error.clone());
                            }
                        }
                        return;
                    },
                }
                // Pages waiting on the catalog.
                let waiting: Vec<WebViewId> = self
                    .pages
                    .iter()
                    .filter(|(_, page)| page.status == Status::Downloading)
                    .map(|(id, _)| *id)
                    .collect();
                for id in waiting {
                    if let Some(webview) = find(id) {
                        self.start(&webview);
                    }
                }
            },
            TranslateEvent::Downloaded { pair, result } => {
                self.downloading.remove(&pair);
                let waiting: Vec<WebViewId> = self
                    .pages
                    .iter()
                    .filter(|(_, page)| page.status == Status::Downloading)
                    .map(|(id, _)| *id)
                    .collect();
                match result {
                    Ok(()) => {
                        for id in waiting {
                            if let Some(webview) = find(id) {
                                self.start(&webview);
                            }
                        }
                    },
                    Err(error) => {
                        warn!("Downloading translation model {pair} failed: {error}");
                        for id in waiting {
                            self.set_status(id, Status::Failed(error.clone()));
                        }
                    },
                }
            },
        }
    }

    fn poll_later(&self, webview: WebViewId, generation: u64) {
        let Some(send) = self.sender() else {
            return;
        };
        std::thread::Builder::new()
            .name("TranslatePoll".into())
            .spawn(move || {
                std::thread::sleep(POLL);
                send(TranslateEvent::Poll {
                    webview,
                    generation,
                });
            })
            .ok();
    }

    fn show_original(&mut self, webview: &WebView) {
        let generation = self.next_generation;
        self.next_generation += 1;
        if let Some(page) = self.pages.get_mut(&webview.id()) {
            // A new generation: batches still out are dropped.
            page.generation = generation;
            page.busy = false;
            page.status = Status::Offer;
        }
        webview.evaluate_javascript(
            "window.__stillwaterTranslate && window.__stillwaterTranslate.restore()".to_owned(),
            |_| {},
        );
    }

    fn toggle(list: &mut Vec<String>, item: &str) {
        if let Some(index) = list.iter().position(|existing| existing == item) {
            list.remove(index);
        } else {
            list.push(item.to_owned());
        }
    }

    /// A `tr-*` command from the address bar or settings page.
    pub(crate) fn command(&mut self, command: &str, argument: &str, page: Option<WebView>) {
        let from = page
            .as_ref()
            .and_then(|webview| self.pages.get(&webview.id()))
            .and_then(|page| page.from.clone());
        let host = page
            .as_ref()
            .and_then(|webview| self.pages.get(&webview.id()))
            .map(|page| page.host.clone())
            .unwrap_or_default();
        match command {
            "tr-translate" => {
                if let Some(webview) = &page {
                    self.start(webview);
                }
            },
            "tr-original" => {
                if let Some(webview) = &page {
                    self.show_original(webview);
                }
            },
            "tr-always" => {
                if let Some(from) = from {
                    Self::toggle(&mut self.prefs.always, &from);
                    self.prefs
                        .never_languages
                        .retain(|language| *language != from);
                    self.save();
                    if self.prefs.always.contains(&from) {
                        if let Some(webview) = &page {
                            self.start(webview);
                        }
                    }
                }
            },
            "tr-never-language" => {
                if let Some(from) = from {
                    Self::toggle(&mut self.prefs.never_languages, &from);
                    self.prefs.always.retain(|language| *language != from);
                    self.save();
                    if self.prefs.never_languages.contains(&from) {
                        if let Some(webview) = &page {
                            self.show_original(webview);
                        }
                    }
                }
            },
            "tr-never-site" => {
                if !host.is_empty() {
                    Self::toggle(&mut self.prefs.never_sites, &host);
                    self.save();
                    if self.prefs.never_sites.contains(&host) {
                        if let Some(webview) = &page {
                            self.show_original(webview);
                        }
                    }
                }
            },
            "tr-auto" => {
                self.prefs.auto = argument == "true";
                self.save();
            },
            "tr-target" => {
                if let Some(code) = normalize_tag(argument) {
                    self.prefs.target = code;
                    self.save();
                }
            },
            // `tr-unlist` "always:de", "never-language:fr", "never-site:example.com"
            "tr-unlist" => {
                if let Some((list, item)) = argument.split_once(':') {
                    match list {
                        "always" => self.prefs.always.retain(|entry| entry != item),
                        "never-language" => {
                            self.prefs.never_languages.retain(|entry| entry != item)
                        },
                        "never-site" => self.prefs.never_sites.retain(|entry| entry != item),
                        _ => {},
                    }
                    self.save();
                }
            },
            "tr-delete-model" => {
                let valid = argument.split_once('-').is_some_and(|(from, to)| {
                    normalize_tag(from).is_some() && normalize_tag(to).is_some()
                });
                if valid {
                    if let Ok(mut engines) = self.engines.lock() {
                        engines.retain(|(pair, _)| pair != argument);
                    }
                    if let Some(dir) = model_dir(argument) {
                        let _ = std::fs::remove_dir_all(dir);
                    }
                }
            },
            "tr-models" => self.ensure_catalog(),
            _ => {},
        }
    }

    /// The address bar's view of a page, and the settings page's of everything.
    pub(crate) fn to_json(&self, page: Option<WebViewId>) -> Value {
        let page = page.and_then(|id| self.pages.get(&id));
        let from = page.and_then(|page| page.from.clone());
        let (status, message) = match page.map(|page| &page.status) {
            None | Some(Status::Idle) | Some(Status::Detecting) => ("idle", String::new()),
            Some(Status::Same) => ("same", String::new()),
            Some(Status::Offer) => ("offer", String::new()),
            Some(Status::Downloading) => ("downloading", String::new()),
            Some(Status::Translating) => ("translating", String::new()),
            Some(Status::Translated) => ("translated", String::new()),
            Some(Status::Unsupported) => ("unsupported", String::new()),
            Some(Status::Failed(error)) => ("failed", error.clone()),
        };
        let host = page.map(|page| page.host.clone()).unwrap_or_default();
        let models: Vec<Value> = translations_dir()
            .map(|dir| dir.join("models"))
            .and_then(|dir| std::fs::read_dir(dir).ok())
            .into_iter()
            .flatten()
            .flatten()
            .filter(|entry| entry.path().join("ready").exists())
            .filter_map(|entry| {
                let pair = entry.file_name().to_string_lossy().into_owned();
                let (from, to) = pair
                    .split_once('-')
                    .map(|(a, b)| (a.to_owned(), b.to_owned()))?;
                let size: u64 = std::fs::read_dir(entry.path())
                    .ok()?
                    .flatten()
                    .filter_map(|file| file.metadata().ok())
                    .map(|meta| meta.len())
                    .sum();
                Some(json!({
                    "pair": pair,
                    "name": format!("{} to {}", language_name(&from), language_name(&to)),
                    "size": size,
                }))
            })
            .collect();
        let targets: Vec<Value> = {
            let mut codes: Vec<&str> = match &self.catalog {
                Some(catalog) => {
                    let mut codes: Vec<&str> = catalog
                        .keys()
                        .filter_map(|pair| pair.strip_prefix("en-"))
                        .collect();
                    codes.push("en");
                    codes
                },
                None => vec![self.prefs.target.as_str()],
            };
            if !codes.contains(&self.prefs.target.as_str()) {
                codes.push(&self.prefs.target);
            }
            let mut targets: Vec<(String, String)> = codes
                .into_iter()
                .map(|code| (code.to_owned(), language_name(code)))
                .collect();
            targets.sort_by(|a, b| a.1.cmp(&b.1));
            targets.dedup();
            targets
                .into_iter()
                .map(|(code, name)| json!({ "code": code, "name": name }))
                .collect()
        };
        json!({
            "status": status,
            "message": message,
            "from": from,
            "fromName": from.as_deref().map(language_name),
            "to": self.prefs.target,
            "toName": language_name(&self.prefs.target),
            "host": host,
            "auto": self.prefs.auto,
            "alwaysThis": from.as_ref().is_some_and(|from| self.prefs.always.contains(from)),
            "neverThis": from.as_ref().is_some_and(|from| self.prefs.never_languages.contains(from)),
            "neverSite": !host.is_empty() && self.prefs.never_sites.contains(&host),
            "always": self.prefs.always.iter().map(|code| json!({ "code": code, "name": language_name(code) })).collect::<Vec<_>>(),
            "neverLanguages": self.prefs.never_languages.iter().map(|code| json!({ "code": code, "name": language_name(code) })).collect::<Vec<_>>(),
            "neverSites": self.prefs.never_sites,
            "models": models,
            "targets": targets,
            "catalogError": self.catalog_status,
        })
    }
}

/// A loaded language pair, loading it the first time (the most recently used few stay loaded).
fn engine(engines: &Engines, pair: &str) -> Result<Arc<Translator>, String> {
    {
        let mut loaded = engines
            .lock()
            .map_err(|_| "Translation engine is unavailable.".to_owned())?;
        if let Some(index) = loaded.iter().position(|(name, _)| name == pair) {
            let entry = loaded.remove(index);
            let engine = entry.1.clone();
            loaded.push(entry);
            return Ok(engine);
        }
    }
    let dir = model_dir(pair).ok_or("No profile folder.")?;
    let engine =
        Arc::new(Translator::from_folder(&dir).map_err(|error| {
            format!("The {pair} translation model couldn't be loaded: {error}")
        })?);
    let mut loaded = engines
        .lock()
        .map_err(|_| "Translation engine is unavailable.".to_owned())?;
    loaded.push((pair.to_owned(), engine.clone()));
    if loaded.len() > LOADED_PAIRS {
        loaded.remove(0);
    }
    Ok(engine)
}

#[cfg(test)]
mod tests {
    use super::{detect, normalize_tag};

    #[test]
    fn model_filters() {
        use super::for_desktop_release;
        assert!(for_desktop_release(""));
        assert!(for_desktop_release(
            "env.appinfo.OS != 'Android' || env.channel != 'release'"
        ));
        assert!(!for_desktop_release("env.appinfo.OS == 'Android' "));
        assert!(!for_desktop_release(
            "env.channel == 'default' || env.channel == 'nightly'"
        ));
        assert!(!for_desktop_release(
            "env.version|versionCompare('130') >= 0"
        ));
    }

    #[test]
    fn language_tags() {
        assert_eq!(normalize_tag("de-DE").as_deref(), Some("de"));
        assert_eq!(normalize_tag("zh-TW").as_deref(), Some("zh-Hant"));
        assert_eq!(normalize_tag("zh_CN").as_deref(), Some("zh-Hans"));
        assert_eq!(normalize_tag("iw").as_deref(), Some("he"));
        assert_eq!(normalize_tag("xx").as_deref(), None);
    }

    #[test]
    fn detects_from_text_over_declared() {
        let german = "Die Bundesregierung hat am Mittwoch ein neues Gesetz zum Klimaschutz beschlossen, das ab dem nächsten Jahr gilt.";
        assert_eq!(detect("en", german).as_deref(), Some("de"));
        assert_eq!(detect("fr", "").as_deref(), Some("fr"));
    }
}
