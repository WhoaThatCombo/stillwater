/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! The developer inspector (F12): a panel docked beside or below the active tab showing its
//! DOM tree, styles, console, network requests and sources, with live editing.
//!
//! The panel is a browser page (`servo:inspector/index.html`) in its own pane. It never touches
//! the inspected page itself; the shell relays its requests:
//!
//! Panel -> Rust: `console.log("__inspector__" + JSON)` (see `PaneDelegate`).
//! Rust -> page: one inspector agent (`inspector-agent.js`) evaluated in the inspected page on
//!   the first request, kept under a random, non-enumerable property of its window. It holds
//!   node ids, the highlight overlay, the element picker and a MutationObserver.
//! Rust -> Panel: `window.inspector.reply(id, json)` and `window.inspector.push(json)`.
//!
//! Nothing runs in a page unless the inspector is open on it, and closing the inspector (or
//! switching tabs) removes the agent with its overlay and listeners. Nothing is saved to disk.

use std::cell::{Cell, RefCell};
use std::hash::{BuildHasher, Hasher};

use serde_json::{Value, json};
use servo::{ConsoleLogLevel, JSValue, WebView, WebViewId};

use crate::desktop::panes::Pane;

/// Where the inspector's page lives; it may not navigate elsewhere.
pub(crate) const PAGE_PREFIX: &str = "servo:inspector/";
pub(crate) const PAGE_URL: &str = "servo:inspector/index.html";
/// Messages from the inspector page to the shell start with this.
pub(crate) const MESSAGE_PREFIX: &str = "__inspector__";

const AGENT_JS: &str = include_str!("inspector-agent.js");
/// What a request returns when the page has no agent yet (a new tab or a navigation).
const NO_AGENT: &str = "\u{1}noagent";
/// Console messages kept for the panel between updates (more are dropped).
const CONSOLE_LIMIT: usize = 500;

/// Asked for by keyboard shortcuts and the page's context menu; handled on the next update.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Request {
    /// F12 / Ctrl+Shift+I: open or close.
    Toggle,
    /// Ctrl+Shift+C: open if needed and pick an element in the page.
    Pick,
    /// "Inspect" on the page's context menu: open and select the element at this point
    /// (CSS px in the page).
    InspectAt(f32, f32),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Dock {
    Right,
    Bottom,
}

/// Work left by script-evaluation callbacks, which can't evaluate more script themselves (the
/// engine's evaluator is busy while it runs them): done on the next update.
enum Deferred {
    /// Answer request `id` in the panel.
    Reply(u64, String),
    /// The page had no agent: evaluate this (the agent and the request) in `WebView`.
    Retry(u64, WebView, String),
    /// Send a message to the panel.
    Push(Value),
}

thread_local! {
    static DEFERRED: RefCell<Vec<Deferred>> = const { RefCell::new(Vec::new()) };
    static REQUESTS: RefCell<Vec<Request>> = const { RefCell::new(Vec::new()) };
    /// The inspected tab and its console messages since the last update.
    static CONSOLE: RefCell<(Option<WebViewId>, Vec<Value>)> = const { RefCell::new((None, Vec::new())) };
    /// Dock side, width (docked right) and height (docked bottom), in device-independent
    /// pixels: kept while the browser runs, never saved.
    static LAYOUT: Cell<(Dock, f32, f32)> = const { Cell::new((Dock::Right, 460.0, 320.0)) };
}

pub(crate) fn request(request: Request) {
    REQUESTS.with(|requests| requests.borrow_mut().push(request));
}

pub(crate) fn take_requests() -> Vec<Request> {
    REQUESTS.with(|requests| std::mem::take(&mut *requests.borrow_mut()))
}

/// A console message from a tab: kept for the inspector if it is open on that tab. Returns
/// true if it was kept. Costs one thread-local lookup when the inspector is closed.
pub(crate) fn on_console(webview: WebViewId, level: &ConsoleLogLevel, message: &str) -> bool {
    CONSOLE.with(|console| {
        let mut console = console.borrow_mut();
        if console.0 != Some(webview) {
            return false;
        }
        if console.1.len() < CONSOLE_LIMIT {
            let level = match level {
                ConsoleLogLevel::Error => "error",
                ConsoleLogLevel::Warn => "warn",
                ConsoleLogLevel::Info => "info",
                ConsoleLogLevel::Debug | ConsoleLogLevel::Trace => "debug",
                _ => "log",
            };
            console.1.push(json!({ "level": level, "text": message }));
        }
        true
    })
}

fn set_console_target(target: Option<WebViewId>) {
    CONSOLE.with(|console| *console.borrow_mut() = (target, Vec::new()));
}

fn take_console() -> Vec<Value> {
    CONSOLE.with(|console| std::mem::take(&mut console.borrow_mut().1))
}

pub(crate) fn dock() -> Dock {
    LAYOUT.with(|layout| layout.get().0)
}

/// The panel's width (docked right) or height (docked bottom).
pub(crate) fn size() -> f32 {
    LAYOUT.with(|layout| match layout.get() {
        (Dock::Right, width, _) => width,
        (Dock::Bottom, _, height) => height,
    })
}

pub(crate) fn set_size(size: f32) {
    LAYOUT.with(|layout| {
        let (dock, width, height) = layout.get();
        layout.set(match dock {
            Dock::Right => (dock, size.clamp(240.0, 4000.0), height),
            Dock::Bottom => (dock, width, size.clamp(120.0, 4000.0)),
        });
    });
}

fn set_dock(dock: Dock) {
    LAYOUT.with(|layout| {
        let (_, width, height) = layout.get();
        layout.set((dock, width, height));
    });
}

/// Split `area` (the page area) into the page and the panel, with `gap` between them, which
/// is the panel's resize handle. Returns (page, gap, panel).
pub(crate) fn layout(area: egui::Rect, gap: f32) -> (egui::Rect, egui::Rect, egui::Rect) {
    use egui::{pos2, Rect};
    match dock() {
        Dock::Right => {
            let max = (area.width() - gap - 160.0).max(120.0);
            let width = size().min(max).floor();
            let panel = Rect::from_min_max(pos2(area.max.x - width, area.min.y), area.max);
            let gap_rect = Rect::from_min_max(
                pos2(panel.min.x - gap, area.min.y),
                pos2(panel.min.x, area.max.y),
            );
            let page = Rect::from_min_max(area.min, pos2(gap_rect.min.x, area.max.y));
            (page, gap_rect, panel)
        },
        Dock::Bottom => {
            let max = (area.height() - gap - 120.0).max(100.0);
            let height = size().min(max).floor();
            let panel = Rect::from_min_max(pos2(area.min.x, area.max.y - height), area.max);
            let gap_rect = Rect::from_min_max(
                pos2(area.min.x, panel.min.y - gap),
                pos2(area.max.x, panel.min.y),
            );
            let page = Rect::from_min_max(area.min, pos2(area.max.x, gap_rect.min.y));
            (page, gap_rect, panel)
        },
    }
}

/// A random name for the agent's property on the page's window.
fn random_key() -> String {
    let state = std::collections::hash_map::RandomState::new();
    let mut key = String::from("__sw_inspector_");
    for round in 0..2u64 {
        let mut hasher = state.build_hasher();
        hasher.write_u64(round);
        hasher.write_u128(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|time| time.as_nanos())
                .unwrap_or_default(),
        );
        key.push_str(&format!("{:016x}", hasher.finish()));
    }
    key
}

pub(crate) struct Inspector {
    pub(crate) pane: Pane,
    /// The tab being inspected.
    target: Option<WebView>,
    /// Its address when last seen, to tell the panel about navigations.
    target_url: Option<String>,
    key: String,
    /// The panel page has loaded.
    ready: bool,
    /// Pushes waiting for the panel page to load.
    queued: Vec<Value>,
    /// The panel asked to be closed.
    pub(crate) wants_close: bool,
    /// Console messages to show first (from before the inspector opened).
    pending_console: Vec<Value>,
}

impl Inspector {
    pub(crate) fn new(pane: Pane) -> Self {
        crate::desktop::diagnostics::log("inspector opened");
        Self {
            pane,
            target: None,
            target_url: None,
            key: random_key(),
            ready: false,
            queued: Vec::new(),
            wants_close: false,
            pending_console: Vec::new(),
        }
    }

    /// Inspect `target` (the active tab), moving off the previous one if it changed, and tell
    /// the panel when the page navigates.
    pub(crate) fn sync_target(
        &mut self,
        target: Option<WebView>,
        state: &crate::running_app_state::RunningAppState,
    ) {
        let same = self.target.as_ref().map(WebView::id) == target.as_ref().map(WebView::id);
        if !same {
            self.remove_agent();
            set_console_target(target.as_ref().map(WebView::id));
            // The errors and warnings the shell already kept for the tab (for "Report a broken
            // site"), so the console isn't empty of what happened before it opened.
            let earlier = target
                .as_ref()
                .map(|webview| state.recent_console_errors(webview.id()))
                .unwrap_or_default();
            self.target = target;
            self.target_url = None;
            if !earlier.is_empty() {
                let mut items = vec![json!({ "level": "info", "text": "Errors and warnings from before the inspector opened:" })];
                items.extend(earlier.into_iter().map(|text| json!({ "level": "warn", "text": text })));
                self.pending_console = items;
            }
        }
        let url = self.target.as_ref().and_then(|webview| webview.url()).map(|url| url.to_string());
        if !same || url != self.target_url {
            self.target_url = url.clone();
            let title = self.target.as_ref().and_then(|webview| webview.page_title());
            self.push(json!({ "type": "target", "url": url, "title": title, "tab": !same }));
        }
        let mut console = std::mem::take(&mut self.pending_console);
        console.extend(take_console());
        if !console.is_empty() {
            self.push(json!({ "type": "console", "items": console }));
        }
    }

    /// Send `message` to the panel page (or keep it until the page is ready).
    pub(crate) fn push(&mut self, message: Value) {
        if !self.ready {
            self.queued.push(message);
            return;
        }
        self.pane
            .webview
            .evaluate_javascript(format!("window.inspector&&window.inspector.push({message})"), |_| {});
    }

    /// Whether script results are waiting for the next update.
    pub(crate) fn has_results(&self) -> bool {
        DEFERRED.with(|deferred| !deferred.borrow().is_empty())
    }

    /// Hand script results to the panel, and add the agent where a request found none.
    fn handle_results(&mut self) {
        let results = DEFERRED.with(|deferred| std::mem::take(&mut *deferred.borrow_mut()));
        for result in results {
            match result {
                Deferred::Reply(id, text) => {
                    let text = Value::from(text);
                    self.pane.webview.evaluate_javascript(
                        format!("window.inspector&&window.inspector.reply({id},{text})"),
                        |_| {},
                    );
                },
                Deferred::Retry(id, target, script) => {
                    let waker = self.pane.waker();
                    target.evaluate_javascript(script, move |result| {
                        defer(&waker, Deferred::Reply(id, result_text(result)))
                    });
                },
                Deferred::Push(message) => self.push(message),
            }
        }
    }

    /// Handle what the panel page sent since the last update.
    pub(crate) fn handle_messages(&mut self) {
        self.handle_results();
        for message in self.pane.take_messages() {
            let Ok(message) = serde_json::from_str::<Value>(&message) else {
                continue;
            };
            match message["cmd"].as_str().unwrap_or_default() {
                "ready" => {
                    self.ready = true;
                    let mut queued = std::mem::take(&mut self.queued);
                    queued.insert(0, json!({ "type": "dock", "dock": self.dock_name() }));
                    let url = self.target_url.clone();
                    let title = self.target.as_ref().and_then(|webview| webview.page_title());
                    queued.insert(1, json!({ "type": "target", "url": url, "title": title, "tab": true }));
                    for message in queued {
                        self.push(message);
                    }
                    // Tests: STILLWATER_TEST_INSPECTOR_SCRIPT runs in the panel page once it's
                    // ready, to drive it the way clicks do.
                    if let Ok(script) = std::env::var("STILLWATER_TEST_INSPECTOR_SCRIPT") {
                        self.pane.webview.evaluate_javascript(script, |_| {});
                    }
                },
                "call" => {
                    let id = message["id"].as_u64().unwrap_or_default();
                    let method = message["method"].as_str().unwrap_or_default();
                    self.call(id, method, &message["params"]);
                },
                "raw-eval" => self.raw_eval(message["code"].as_str().unwrap_or_default()),
                "close" => self.wants_close = true,
                "dock" => {
                    set_dock(if message["dock"] == "bottom" {
                        Dock::Bottom
                    } else {
                        Dock::Right
                    });
                    let dock = self.dock_name();
                    self.push(json!({ "type": "dock", "dock": dock }));
                },
                "debug" => crate::desktop::diagnostics::log(format!(
                    "inspector debug: {}",
                    message["text"].as_str().unwrap_or_default()
                )),
                _ => {},
            }
        }
    }

    fn dock_name(&self) -> &'static str {
        match dock() {
            Dock::Right => "right",
            Dock::Bottom => "bottom",
        }
    }

    /// Run `method` in the inspected page's agent (adding the agent first if the page has
    /// none) and answer request `id` in the panel.
    fn call(&self, id: u64, method: &str, params: &Value) {
        let waker = self.pane.waker();
        let Some(target) = self.target.clone() else {
            defer(&waker, Deferred::Reply(id, json!({ "error": "No page to inspect" }).to_string()));
            return;
        };
        let key = Value::from(self.key.clone()).to_string();
        let method = Value::from(method).to_string();
        let params = if params.is_null() { json!({}) } else { params.clone() };
        let invoke = format!(
            "(function(){{var a=Object.getOwnPropertyDescriptor(window,{key});\
             return a?a.value.call({method},{params}):{}}})()",
            Value::from(NO_AGENT)
        );
        let with_agent = format!("{};{invoke}", AGENT_JS.replace("__KEY__", &self.key));
        let retry_target = target.clone();
        target.evaluate_javascript(invoke, move |result| match result {
            Ok(JSValue::String(text)) if text == NO_AGENT => {
                defer(&waker, Deferred::Retry(id, retry_target, with_agent))
            },
            result => defer(&waker, Deferred::Reply(id, result_text(result))),
        });
    }

    /// Console input on a page whose Content-Security-Policy blocks `eval`: run it as an
    /// embedder script instead, and show the value as the engine reports it.
    fn raw_eval(&self, code: &str) {
        let Some(target) = &self.target else {
            return;
        };
        let waker = self.pane.waker();
        target.evaluate_javascript(code.to_owned(), move |result| {
            let message = match result {
                Ok(value) => json!({ "type": "raw-result", "value": preview(&value, 0) }),
                Err(error) => json!({ "type": "raw-result", "error": format!("{error:?}") }),
            };
            defer(&waker, Deferred::Push(message));
        });
    }

    /// Remove the agent (overlay, listeners, observer) from the inspected page.
    fn remove_agent(&self) {
        if let Some(target) = &self.target {
            let key = Value::from(self.key.clone()).to_string();
            target.evaluate_javascript(
                format!(
                    "(function(){{var a=Object.getOwnPropertyDescriptor(window,{key});\
                     if(a)a.value.call('destroy',{{}});}})()"
                ),
                |_| {},
            );
        }
    }
}

impl Drop for Inspector {
    fn drop(&mut self) {
        self.remove_agent();
        set_console_target(None);
        DEFERRED.with(|deferred| deferred.borrow_mut().clear());
        crate::desktop::diagnostics::log("inspector closed");
    }
}

/// A value from the engine in the panel's console format (see `preview` in the agent).
fn preview(value: &JSValue, depth: usize) -> Value {
    let entries = |items: Vec<(String, &JSValue)>| -> Vec<Value> {
        items
            .into_iter()
            .take(100)
            .map(|(key, item)| json!([key, preview(item, depth + 1)]))
            .collect()
    };
    match value {
        JSValue::Undefined => json!({ "t": "undefined" }),
        JSValue::Null => json!({ "t": "null" }),
        JSValue::Boolean(value) => json!({ "t": "boolean", "v": value.to_string() }),
        JSValue::Number(value) => json!({ "t": "number", "v": value.to_string() }),
        JSValue::String(value) => json!({ "t": "string", "v": value }),
        JSValue::Element(_) => json!({ "t": "object", "c": "Element", "n": 0 }),
        JSValue::ShadowRoot(_) => json!({ "t": "object", "c": "ShadowRoot", "n": 0 }),
        JSValue::Frame(_) => json!({ "t": "object", "c": "Window (frame)", "n": 0 }),
        JSValue::Window(_) => json!({ "t": "object", "c": "Window", "n": 0 }),
        JSValue::Array(items) => {
            let mut result = json!({ "t": "array", "c": "Array", "n": items.len() });
            if depth < 2 {
                result["e"] = entries(items.iter().enumerate().map(|(i, v)| (i.to_string(), v)).collect()).into();
            }
            result
        },
        JSValue::Object(map) => {
            let mut result = json!({ "t": "object", "c": "Object", "n": map.len() });
            if depth < 2 {
                let mut items: Vec<_> = map.iter().map(|(k, v)| (k.clone(), v)).collect();
                items.sort_by(|a, b| a.0.cmp(&b.0));
                result["e"] = entries(items).into();
            }
            result
        },
    }
}

fn result_text(result: Result<JSValue, servo::JavaScriptEvaluationError>) -> String {
    match result {
        Ok(JSValue::String(text)) => text,
        Ok(other) => json!({ "error": format!("Unexpected answer from the page: {other:?}") }).to_string(),
        Err(error) => json!({ "error": format!("The page didn't answer: {error:?}") }).to_string(),
    }
}

fn defer(waker: &std::task::Waker, work: Deferred) {
    DEFERRED.with(|deferred| deferred.borrow_mut().push(work));
    waker.wake_by_ref();
}
