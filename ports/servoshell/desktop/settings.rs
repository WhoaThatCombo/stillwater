/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Stillwater's user settings (the sidebar's gear panel), saved as
//! `<config dir>/stillwater-settings.json` next to `workspaces.json`.

use std::path::PathBuf;
use std::sync::RwLock;
use std::sync::atomic::{AtomicBool, Ordering};

use log::warn;
use serde_json::{Value, json};
use servo::Theme;

/// Search engines offered in settings: (id, name, URL template with `%s` for the query).
const SEARCH_ENGINES: &[(&str, &str, &str)] = &[
    ("duckduckgo", "DuckDuckGo", "https://duckduckgo.com/?q=%s"),
    ("google", "Google", "https://www.google.com/search?q=%s"),
    ("bing", "Bing", "https://www.bing.com/search?q=%s"),
    ("brave", "Brave Search", "https://search.brave.com/search?q=%s"),
];

/// The current search URL template, readable from the protocol handler thread so the
/// New Tab page (`servo:search-engine.js`) searches with the same engine.
static SEARCH_TEMPLATE: RwLock<String> = RwLock::new(String::new());

/// The "Smooth scrolling" setting, readable where wheel events are made.
static SMOOTH_SCROLL: AtomicBool = AtomicBool::new(true);

/// Whether a mouse-wheel notch animates its scroll (Paint eases it over ~130 ms, as Edge
/// does) instead of jumping: the "Smooth scrolling" setting, unless `STILLWATER_SMOOTH_SCROLL`
/// is set (`0` off, anything else on).
pub(crate) fn smooth_scroll_enabled() -> bool {
    static ENV: std::sync::LazyLock<Option<bool>> = std::sync::LazyLock::new(|| {
        std::env::var_os("STILLWATER_SMOOTH_SCROLL").map(|value| value != "0")
    });
    ENV.unwrap_or_else(|| SMOOTH_SCROLL.load(Ordering::Relaxed))
}

/// Where Stillwater keeps its settings, spaces and pins. `STILLWATER_CONFIG_DIR` overrides it
/// (used to test without touching a real profile).
pub(crate) fn config_dir() -> Option<PathBuf> {
    std::env::var_os("STILLWATER_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(crate::prefs::default_config_dir)
}

pub(crate) fn search_template() -> String {
    let template = SEARCH_TEMPLATE
        .read()
        .map(|template| template.clone())
        .unwrap_or_default();
    if template.is_empty() {
        SEARCH_ENGINES[0].2.to_owned()
    } else {
        template
    }
}

/// The name of the search engine chosen in settings.
pub(crate) fn search_engine_name() -> &'static str {
    let template = search_template();
    SEARCH_ENGINES
        .iter()
        .find(|(.., engine_template)| *engine_template == template)
        .map_or(SEARCH_ENGINES[0].1, |(_, name, _)| name)
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum ThemeChoice {
    System,
    Light,
    Dark,
}

impl ThemeChoice {
    fn id(self) -> &'static str {
        match self {
            ThemeChoice::System => "system",
            ThemeChoice::Light => "light",
            ThemeChoice::Dark => "dark",
        }
    }

    pub(crate) fn from_id(id: &str) -> Option<Self> {
        match id {
            "system" => Some(ThemeChoice::System),
            "light" => Some(ThemeChoice::Light),
            "dark" => Some(ThemeChoice::Dark),
            _ => None,
        }
    }

    /// The theme to force, or `None` to follow Windows.
    pub(crate) fn override_theme(self) -> Option<Theme> {
        match self {
            ThemeChoice::System => None,
            ThemeChoice::Light => Some(Theme::Light),
            ThemeChoice::Dark => Some(Theme::Dark),
        }
    }
}

/// "Load my tabs in the background at startup": which of the last session's tabs load by
/// themselves after launch (the rest load when clicked).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) enum PreloadMode {
    /// All restored tabs, pinned tabs and Essentials.
    #[default]
    All,
    /// Only pinned tabs and Essentials.
    Pinned,
    /// None: tabs load when clicked.
    Off,
}

impl PreloadMode {
    pub(crate) fn id(self) -> &'static str {
        match self {
            PreloadMode::All => "all",
            PreloadMode::Pinned => "pinned",
            PreloadMode::Off => "off",
        }
    }

    pub(crate) fn from_id(id: &str) -> Option<Self> {
        match id {
            "all" => Some(PreloadMode::All),
            "pinned" => Some(PreloadMode::Pinned),
            "off" => Some(PreloadMode::Off),
            _ => None,
        }
    }
}

/// Choices for "Unload inactive tabs after", in minutes (0 = never).
const TAB_UNLOAD_CHOICES: &[u32] = &[0, 10, 30, 60, 120];
/// Like Zen's tab unloader, background tabs are unloaded after this long by default.
const DEFAULT_TAB_UNLOAD_MINUTES: u32 = 30;

pub(crate) struct Settings {
    pub(crate) theme: ThemeChoice,
    search_engine: String,
    pub(crate) compact: bool,
    /// Unload background tabs that haven't been used for this many minutes (0 = never).
    pub(crate) tab_unload_minutes: u32,
    /// Unload the least recently used background tabs when Windows runs low on memory.
    pub(crate) unload_on_low_memory: bool,
    /// On startup, restore the previous session's tabs (otherwise open a new tab).
    pub(crate) restore_session: bool,
    /// New tab (Ctrl+T) opens Zen's floating address bar instead of a New Tab page.
    pub(crate) new_tab_bar: bool,
    /// Zen's spaces (several sets of tabs, switched at the bottom of the sidebar). Off by
    /// default: one list of tabs.
    pub(crate) spaces: bool,
    /// Play the short animation that covers the window while Stillwater starts.
    pub(crate) startup_animation: bool,
    /// Which restored tabs load in the background right after launch.
    pub(crate) preload_tabs: PreloadMode,
    /// How the page scrollbar shows: "auto" (overlay, fades out), "always" or "hidden".
    pub(crate) scrollbar: crate::desktop::scrollbar::ScrollbarMode,
    /// Animate each mouse-wheel notch instead of jumping (see `smooth_scroll_enabled`).
    pub(crate) smooth_scroll: bool,
    /// Width of the docked sidebar in device-independent pixels (dragged by its right edge).
    pub(crate) sidebar_width: f32,
    path: Option<PathBuf>,
}

impl Settings {
    pub(crate) fn load() -> Self {
        let path = config_dir().map(|dir| dir.join("stillwater-settings.json"));
        let saved = path
            .as_ref()
            .and_then(|path| std::fs::read_to_string(path).ok())
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .unwrap_or(Value::Null);
        let settings = Self {
            theme: saved["theme"]
                .as_str()
                .and_then(ThemeChoice::from_id)
                .unwrap_or(ThemeChoice::System),
            search_engine: saved["searchEngine"]
                .as_str()
                .filter(|id| SEARCH_ENGINES.iter().any(|(engine, ..)| engine == id))
                .unwrap_or(SEARCH_ENGINES[0].0)
                .to_owned(),
            compact: saved["compact"].as_bool().unwrap_or(false),
            tab_unload_minutes: saved["tabUnloadMinutes"]
                .as_u64()
                .map(|minutes| minutes as u32)
                .filter(|minutes| TAB_UNLOAD_CHOICES.contains(minutes))
                .unwrap_or(DEFAULT_TAB_UNLOAD_MINUTES),
            unload_on_low_memory: saved["unloadOnLowMemory"].as_bool().unwrap_or(true),
            restore_session: saved["startup"].as_str() != Some("newtab"),
            new_tab_bar: saved["newTabBar"].as_bool().unwrap_or(true),
            spaces: saved["spaces"].as_bool().unwrap_or(false),
            startup_animation: saved["startupAnimation"].as_bool().unwrap_or(true),
            preload_tabs: saved["preloadTabs"]
                .as_str()
                .and_then(PreloadMode::from_id)
                .unwrap_or_default(),
            scrollbar: saved["scrollbar"]
                .as_str()
                .and_then(crate::desktop::scrollbar::ScrollbarMode::from_id)
                .unwrap_or_default(),
            smooth_scroll: saved["smoothScroll"].as_bool().unwrap_or(true),
            sidebar_width: saved["sidebarWidth"]
                .as_f64()
                .map(|width| width as f32)
                .filter(|width| width.is_finite())
                .unwrap_or(crate::desktop::sidebar::SIDEBAR_WIDTH),
            // Incognito reads the settings but never saves changes (they last for the session).
            path: path.filter(|_| !crate::desktop::incognito::active()),
        };
        settings.publish_search_template();
        crate::desktop::scrollbar::set_mode(settings.scrollbar);
        SMOOTH_SCROLL.store(settings.smooth_scroll, Ordering::Relaxed);
        settings
    }

    fn save(&self) {
        let Some(path) = &self.path else {
            return;
        };
        let data = json!({
            "theme": self.theme.id(),
            "searchEngine": self.search_engine,
            "compact": self.compact,
            "tabUnloadMinutes": self.tab_unload_minutes,
            "unloadOnLowMemory": self.unload_on_low_memory,
            "startup": self.startup_id(),
            "newTabBar": self.new_tab_bar,
            "spaces": self.spaces,
            "startupAnimation": self.startup_animation,
            "preloadTabs": self.preload_tabs.id(),
            "scrollbar": self.scrollbar.id(),
            "smoothScroll": self.smooth_scroll,
            "sidebarWidth": self.sidebar_width,
        });
        let result = path
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|_| std::fs::write(path, data.to_string()));
        if let Err(error) = result {
            warn!("Could not save settings to {}: {error}", path.display());
        }
    }

    fn publish_search_template(&self) {
        if let Ok(mut template) = SEARCH_TEMPLATE.write() {
            *template = self.search_url_template().to_owned();
        }
    }

    /// The URL template for searches, with `%s` where the query goes.
    pub(crate) fn search_url_template(&self) -> &'static str {
        SEARCH_ENGINES
            .iter()
            .find(|(id, ..)| *id == self.search_engine)
            .map_or(SEARCH_ENGINES[0].2, |(.., template)| template)
    }

    pub(crate) fn set_theme(&mut self, theme: ThemeChoice) {
        self.theme = theme;
        self.save();
    }

    pub(crate) fn set_search_engine(&mut self, id: &str) {
        if SEARCH_ENGINES.iter().any(|(engine, ..)| *engine == id) {
            self.search_engine = id.to_owned();
            self.publish_search_template();
            self.save();
        }
    }

    pub(crate) fn set_compact(&mut self, compact: bool) {
        if self.compact != compact {
            self.compact = compact;
            self.save();
        }
    }

    pub(crate) fn set_sidebar_width(&mut self, width: f32) {
        if self.sidebar_width != width {
            self.sidebar_width = width;
            self.save();
        }
    }

    pub(crate) fn set_tab_unload_minutes(&mut self, minutes: u32) {
        if TAB_UNLOAD_CHOICES.contains(&minutes) {
            self.tab_unload_minutes = minutes;
            self.save();
        }
    }

    pub(crate) fn set_unload_on_low_memory(&mut self, enabled: bool) {
        self.unload_on_low_memory = enabled;
        self.save();
    }

    fn startup_id(&self) -> &'static str {
        if self.restore_session { "restore" } else { "newtab" }
    }

    /// "restore" (the previous session's tabs) or "newtab".
    pub(crate) fn set_startup(&mut self, id: &str) {
        if matches!(id, "restore" | "newtab") {
            self.restore_session = id == "restore";
            self.save();
        }
    }

    pub(crate) fn set_spaces(&mut self, enabled: bool) {
        self.spaces = enabled;
        self.save();
    }

    pub(crate) fn set_startup_animation(&mut self, enabled: bool) {
        self.startup_animation = enabled;
        self.save();
    }

    pub(crate) fn set_preload_tabs(&mut self, id: &str) {
        if let Some(mode) = PreloadMode::from_id(id) {
            self.preload_tabs = mode;
            self.save();
        }
    }

    pub(crate) fn set_scrollbar(&mut self, id: &str) {
        if let Some(mode) = crate::desktop::scrollbar::ScrollbarMode::from_id(id) {
            self.scrollbar = mode;
            crate::desktop::scrollbar::set_mode(mode);
            self.save();
        }
    }

    pub(crate) fn set_smooth_scroll(&mut self, enabled: bool) {
        self.smooth_scroll = enabled;
        SMOOTH_SCROLL.store(enabled, Ordering::Relaxed);
        self.save();
    }

    pub(crate) fn set_new_tab_bar(&mut self, enabled: bool) {
        self.new_tab_bar = enabled;
        self.save();
    }

    pub(crate) fn to_json(&self) -> Value {
        let engines: Vec<Value> = SEARCH_ENGINES
            .iter()
            .map(|(id, name, _)| json!({ "id": id, "name": name }))
            .collect();
        json!({
            "theme": self.theme.id(),
            "searchEngine": self.search_engine,
            "searchEngines": engines,
            "tabUnloadMinutes": self.tab_unload_minutes,
            "tabUnloadChoices": TAB_UNLOAD_CHOICES,
            "unloadOnLowMemory": self.unload_on_low_memory,
            "startup": self.startup_id(),
            "newTabBar": self.new_tab_bar,
            "spaces": self.spaces,
            "startupAnimation": self.startup_animation,
            "preloadTabs": self.preload_tabs.id(),
            "scrollbar": self.scrollbar.id(),
            "smoothScroll": self.smooth_scroll,
            "version": crate::desktop::updater::CURRENT_VERSION,
        })
    }
}
