/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Loading the last session's tabs in the background at startup, so they are ready when
//! clicked (Settings → General → On startup → "Load my tabs in the background at startup").
//!
//! Order: the active tab first (it is the window's first tab, as before), then the other tabs
//! of the shown space, most recently used first (list order for sessions saved before 0.9),
//! then that space's pinned tabs and the Essentials, then the other spaces' tabs and pinned
//! tabs. Only `All` mode loads ordinary tabs; `Pinned` loads only pinned tabs and Essentials.
//!
//! Throttling: nothing starts until the active tab has finished loading (plus `START_GRACE`,
//! or `START_DELAY` has passed), at most `MAX_LOADING` load at once, and the queue waits while the user loads
//! something in the shown tab (for at most `FOREGROUND_PAUSE_LIMIT` per load, in case a page
//! never finishes). A tab the user clicks before its turn loads at once, the normal way.
//!
//! Memory: loading stops for good when Windows is low on memory or when `DEFAULT_LIMIT` tabs
//! are open. The queue is fixed when it starts and each tab is tried once, so a tab the
//! unloader unloads is never loaded again by this. A preloaded tab counts as used when it was
//! loaded, so the inactivity unloader (10 minutes or more) leaves it alone until then, and the
//! low-memory unloader can't run while preloading does (preloading stops first).
//!
//! The tabs load hidden (Servo throttles hidden WebViews: no rendering, slower timers), never
//! focused, with media blocked until the user interacts with them, and until they are first
//! shown they may not open popups, download, move, resize or go fullscreen (`ServoShellWindow::
//! is_untouched_preload`). Their dialogs and permission prompts wait for the tab to be shown.

use std::cmp::Reverse;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

use servo::{LoadStatus, WebViewId};

use super::{ESSENTIALS, UnloadedTab, Workspaces};
use crate::desktop::settings::PreloadMode;
use crate::running_app_state::{PreloadTarget, UserInterfaceCommand};
use crate::window::ServoShellWindow;

/// Start anyway this long after launch if the active tab is still loading.
const START_DELAY: Duration = Duration::from_secs(5);
/// After the active tab has loaded, wait this long before starting.
const START_GRACE: Duration = Duration::from_millis(500);
/// At most this many background loads at a time.
const MAX_LOADING: usize = 2;
/// A background load that hasn't finished after this long stops holding up the queue.
const MAX_LOAD_WAIT: Duration = Duration::from_secs(30);
/// The queue waits at most this long for one load in the shown tab.
const FOREGROUND_PAUSE_LIMIT: Duration = Duration::from_secs(10);
/// Stop once this many tabs are open (`STILLWATER_PRELOAD_LIMIT` overrides it, for tests).
const DEFAULT_LIMIT: usize = 12;

pub(super) struct Preloader {
    created: Instant,
    /// Since when the active tab has been loaded (before loading starts).
    settled_since: Option<Instant>,
    /// What's left to load; `None` until loading has started.
    queue: Option<VecDeque<PreloadTarget>>,
    /// The tab shown when loading started, and whether its first load has finished (until then
    /// it doesn't pause the queue: that's the `START_DELAY` case).
    initial: Option<WebViewId>,
    initial_complete: bool,
    /// Asked for, but the tab list hasn't seen the new WebView yet.
    in_flight: Vec<PreloadTarget>,
    /// Loading in the background, since when.
    loading: Vec<(WebViewId, Instant)>,
    /// The user clicked a tab whose WebView is on its way: show it when it arrives.
    activate_on_arrival: Option<PreloadTarget>,
    /// Since when the shown tab has been loading (for `FOREGROUND_PAUSE_LIMIT`).
    foreground_since: Option<Instant>,
    started: usize,
}

impl Preloader {
    pub(super) fn new() -> Self {
        Self {
            created: Instant::now(),
            settled_since: None,
            queue: None,
            initial: None,
            initial_complete: false,
            in_flight: Vec::new(),
            loading: Vec::new(),
            activate_on_arrival: None,
            foreground_since: None,
            started: 0,
        }
    }
}

fn limit() -> usize {
    std::env::var("STILLWATER_PRELOAD_LIMIT")
        .ok()
        .and_then(|limit| limit.parse().ok())
        .unwrap_or(DEFAULT_LIMIT)
}

fn log(message: String) {
    crate::desktop::diagnostics::log(format!("preload: {message}"));
}

impl Workspaces {
    /// Whether background loading at startup still has work to do.
    pub(crate) fn preload_pending(&self) -> bool {
        self.preload.is_some()
    }

    /// Start background loads at startup when it's their turn (see the module docs). Call it
    /// a few times a second after `sync`. `foreground_busy`: the shown tab is loading. Returns
    /// (more to do, the tab list changed).
    pub(crate) fn preload_tick(
        &mut self,
        window: &ServoShellWindow,
        mode: PreloadMode,
        foreground_busy: bool,
    ) -> (bool, bool) {
        let Some(mut preloader) = self.preload.take() else {
            return (false, false);
        };
        if mode == PreloadMode::Off && preloader.queue.is_none() {
            log("off in settings".into());
            return (false, false);
        }
        let active = window.active_webview().map(|webview| webview.id());
        let now = Instant::now();
        let waited = preloader.created.elapsed();
        let mut changed = false;

        if preloader.queue.is_none() {
            // Wait for the first tab to load, and give it a moment for what its page does right
            // after loading (but not forever).
            let settled = if active.is_none() || foreground_busy {
                preloader.settled_since = None;
                false
            } else {
                preloader.settled_since.get_or_insert(now).elapsed() >= START_GRACE
            };
            if !settled && waited < START_DELAY {
                self.preload = Some(preloader);
                return (true, false);
            }
            let queue = self.preload_order(mode);
            log(format!(
                "starting {} ms after the window opened ({}, active tab loaded={}): {} queued",
                waited.as_millis(),
                mode.id(),
                !foreground_busy,
                queue.len()
            ));
            preloader.queue = Some(queue);
            preloader.initial = active;
            preloader.initial_complete = !foreground_busy;
        }
        if active == preloader.initial && !foreground_busy {
            preloader.initial_complete = true;
        }

        // Finished (or given up on) background loads make room for the next ones.
        preloader.loading.retain(|(id, since)| {
            let loading = window.webview_by_id(*id).is_some_and(|webview| {
                matches!(webview.load_status(), LoadStatus::Started | LoadStatus::HeadParsed)
            });
            if !loading || since.elapsed() >= MAX_LOAD_WAIT {
                log(format!(
                    "finished {id:?} after {} ms{}",
                    since.elapsed().as_millis(),
                    if loading { " (gave up waiting)" } else { "" }
                ));
                changed = true;
                return false;
            }
            true
        });

        // The user is loading something in the shown tab: let that go first.
        let initial_load = active == preloader.initial && !preloader.initial_complete;
        let paused = if foreground_busy && !initial_load {
            let since = *preloader.foreground_since.get_or_insert(now);
            since.elapsed() < FOREGROUND_PAUSE_LIMIT
        } else {
            preloader.foreground_since = None;
            false
        };

        if !paused && mode != PreloadMode::Off {
            let limit = limit();
            while preloader.loading.len() + preloader.in_flight.len() < MAX_LOADING {
                let Some(target) = preloader.queue.as_mut().and_then(VecDeque::pop_front) else {
                    break;
                };
                let open = window.webview_ids().len() + preloader.in_flight.len();
                if open >= limit {
                    log(format!("stopping: {open} tabs open (limit {limit})"));
                    preloader.queue = Some(VecDeque::new());
                    break;
                }
                if crate::desktop::memory::system_memory_low() {
                    log("stopping: memory is low".into());
                    preloader.queue = Some(VecDeque::new());
                    break;
                }
                let Some(url) = self.preload_url(target) else {
                    continue;
                };
                log(format!("loading {url} ({target:?})"));
                window.queue_user_interface_command(UserInterfaceCommand::PreloadWebView(url, target));
                preloader.in_flight.push(target);
                preloader.started += 1;
                changed = true;
            }
        }

        let more = preloader.queue.as_ref().is_some_and(|queue| !queue.is_empty()) ||
            !preloader.in_flight.is_empty() ||
            !preloader.loading.is_empty();
        if more {
            self.preload = Some(preloader);
        } else {
            log(format!(
                "done: {} tabs loaded in the background, {} ms after the window opened",
                preloader.started,
                waited.as_millis()
            ));
        }
        (more, changed)
    }

    /// The order to load the restored tabs in (see the module docs).
    fn preload_order(&self, mode: PreloadMode) -> VecDeque<PreloadTarget> {
        let by_recent_use = |mut tabs: Vec<&UnloadedTab>| {
            tabs.sort_by_key(|tab| (tab.used.is_none(), Reverse(tab.used), tab.order));
            tabs.iter().map(|tab| PreloadTarget::Tab(tab.key)).collect::<Vec<_>>()
        };
        let tabs_where = |shown: bool| {
            by_recent_use(
                self.unloaded
                    .iter()
                    .filter(|tab| self.in_view(tab.space) == shown)
                    .collect(),
            )
        };
        let pins_where = |wanted: &dyn Fn(u32) -> bool| {
            self.pins
                .iter()
                .filter(|pin| pin.webview.is_none() && wanted(pin.space))
                .map(|pin| PreloadTarget::Pin(pin.id))
                .collect::<Vec<_>>()
        };
        let all = mode == PreloadMode::All;
        let mut order = VecDeque::new();
        if all {
            order.extend(tabs_where(true));
        }
        order.extend(pins_where(&|space| space != ESSENTIALS && self.in_view(space)));
        order.extend(pins_where(&|space| space == ESSENTIALS));
        if all {
            order.extend(tabs_where(false));
        }
        order.extend(pins_where(&|space| space != ESSENTIALS && !self.in_view(space)));
        order
    }

    /// The page to load for `target`, if it is still waiting to be loaded.
    fn preload_url(&self, target: PreloadTarget) -> Option<url::Url> {
        let url = match target {
            PreloadTarget::Tab(key) => {
                if self.pending_restore == Some(key) {
                    return None;
                }
                &self.unloaded.iter().find(|tab| tab.key == key)?.url
            },
            PreloadTarget::Pin(id) => {
                if self.pending_pin == Some(id) {
                    return None;
                }
                let pin = self.pins.iter().find(|pin| pin.id == id)?;
                if pin.webview.is_some() {
                    return None;
                }
                &pin.url
            },
        };
        url::Url::parse(url).ok()
    }

    /// The user asked to show `target`. If its background WebView is on its way, show it when
    /// it arrives and return true (the caller must not load it a second time).
    pub(super) fn activate_when_preloaded(&mut self, target: PreloadTarget) -> bool {
        let Some(preloader) = self.preload.as_mut() else {
            return false;
        };
        if !preloader.in_flight.contains(&target) {
            return false;
        }
        preloader.activate_on_arrival = Some(target);
        true
    }

    /// A WebView created by `PreloadWebView` appeared: give it its tab's place (or its pin).
    pub(super) fn adopt_preload(&mut self, window: &ServoShellWindow, id: WebViewId, target: PreloadTarget) {
        let mut activate = false;
        if let Some(preloader) = self.preload.as_mut() {
            preloader.in_flight.retain(|other| *other != target);
            preloader.loading.push((id, Instant::now()));
            activate = preloader.activate_on_arrival == Some(target);
            if activate {
                preloader.activate_on_arrival = None;
            }
        }
        let space = match target {
            PreloadTarget::Tab(key) => {
                match self.unloaded.iter().position(|tab| tab.key == key) {
                    Some(position) => {
                        let tab = self.unloaded.remove(position);
                        self.requested_urls.insert(id, tab.url);
                        self.tab_order.insert(id, tab.order);
                        if let Some(title) = tab.custom_title {
                            self.custom_titles.insert(id, title);
                        }
                        self.restored.push((id, tab.key, tab.favicon));
                        Some(tab.space)
                    },
                    None => None,
                }
            },
            PreloadTarget::Pin(pin_id) => {
                match self.pins.iter_mut().find(|pin| pin.id == pin_id && pin.webview.is_none()) {
                    Some(pin) => {
                        pin.webview = Some(id);
                        Some(pin.space)
                    },
                    None => None,
                }
            },
        };
        // The tab was closed or loaded some other way meanwhile: this WebView isn't needed.
        let Some(space) = space else {
            log(format!("{target:?} went away while loading; closing {id:?}"));
            self.tab_workspace.insert(id, self.current);
            window.queue_user_interface_command(UserInterfaceCommand::CloseWebView(id));
            return;
        };
        self.tab_workspace.insert(id, space);
        if activate {
            if self.enabled && space != ESSENTIALS {
                self.current = space;
            }
            window.activate_webview(id);
        }
    }
}
