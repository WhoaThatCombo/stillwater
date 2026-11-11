/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Application entry point, runs the event loop.

use std::path::Path;
use std::rc::Rc;
use std::time::Instant;
use std::{env, fs};

use servo::protocol_handler::ProtocolRegistry;
use servo::{
    EventLoopWaker, LoadStatus, Opts, Preferences, ServoBuilder, ServoUrl, UserContentManager, UserScript,
};
use url::Url;
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoopProxy};
use winit::window::WindowId;

use super::event_loop::AppEvent;
use crate::desktop::diagnostics;
use crate::desktop::event_loop::ServoShellEventLoop;
use crate::desktop::headed_window::HeadedWindow;
use crate::desktop::headless_window::HeadlessWindow;
use crate::desktop::protocols;
use crate::desktop::tracing::trace_winit_event;
use crate::parser::get_default_url;
use crate::prefs::ServoShellPreferences;
use crate::running_app_state::RunningAppState;
#[cfg(feature = "gamepad")]
use crate::running_app_state::ServoshellGamepadDelegate;
use crate::window::{PlatformWindow, ServoShellWindowId, TopLevelWebViewCreationRequest};

pub(crate) enum AppState {
    Initializing,
    Running(Rc<RunningAppState>),
    ShuttingDown,
}

pub struct App {
    opts: Opts,
    preferences: Preferences,
    servoshell_preferences: ServoShellPreferences,
    waker: Box<dyn EventLoopWaker>,
    event_loop_proxy: Option<EventLoopProxy<AppEvent>>,
    initial_url: ServoUrl,
    /// The first tab reopens the last session's active tab (its media waits for the user).
    initial_url_restored: bool,
    t_start: Instant,
    t: Instant,
    state: AppState,
    /// Keep the event loop ticking until this time, e.g. after another launch handed us a URL:
    /// on Windows the window otherwise may not repaint until it receives its next message.
    tick_until: Option<Instant>,
    /// When the last repaint request was handled, and when a held-back repaint is due (see
    /// `user_event`).
    last_repaint_request: Option<Instant>,
    repaint_at: Option<Instant>,
    /// When to next look for tabs to unload (inactive ones, or any when memory is low).
    next_memory_check: Instant,
    /// When Servo was last told that memory is low (it drops caches, so not too often).
    last_memory_pressure: Option<Instant>,
    /// Whether this process holds a 1 ms Windows timer resolution (see `update_timer_resolution`).
    fine_timers: bool,
    /// Since when the fine timer resolution has been continuously unneeded while held.
    fine_timers_unwanted_since: Option<Instant>,
    /// Restored tabs may still be waiting to load in the background (see
    /// `Sidebar::preload_tick`), and when to look next.
    preloading: bool,
    next_preload_tick: Instant,
}

#[cfg(target_os = "windows")]
#[link(name = "winmm")]
unsafe extern "system" {
    fn timeBeginPeriod(period_ms: u32) -> u32;
    fn timeEndPeriod(period_ms: u32) -> u32;
}

/// How often to look for inactive tabs to unload, and how often while memory is low.
const MEMORY_CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);
const LOW_MEMORY_CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(10);
/// How long the fine timer resolution must have been unneeded before it is given back.
const FINE_TIMER_RELEASE_DELAY: std::time::Duration = std::time::Duration::from_secs(2);
/// How often to start background loads of restored tabs while there are some left.
const PRELOAD_TICK_INTERVAL: std::time::Duration = std::time::Duration::from_millis(200);

impl App {
    pub fn new(
        opts: Opts,
        preferences: Preferences,
        servo_shell_preferences: ServoShellPreferences,
        event_loop: &ServoShellEventLoop,
    ) -> Self {
        // Unless a page was asked for, the first tab is the previous session's active tab.
        let restored_url = crate::desktop::session::init(
            servo_shell_preferences.url.as_deref(),
            servo_shell_preferences.headless,
            servo_shell_preferences.webdriver_port.get().is_some(),
        );
        let initial_url_restored = restored_url.is_some();
        crate::desktop::capture::clean_up();
        let initial_url = restored_url.map(ServoUrl::from_url).unwrap_or_else(|| {
            get_default_url(
                servo_shell_preferences.url.as_deref(),
                env::current_dir().unwrap(),
                |path| fs::metadata(path).is_ok(),
                &servo_shell_preferences,
            )
        });

        let t = Instant::now();
        App {
            opts,
            preferences,
            servoshell_preferences: servo_shell_preferences,
            waker: event_loop.create_event_loop_waker(),
            event_loop_proxy: event_loop.event_loop_proxy(),
            initial_url,
            initial_url_restored,
            t_start: t,
            t,
            state: AppState::Initializing,
            tick_until: None,
            last_repaint_request: None,
            repaint_at: None,
            next_memory_check: t + MEMORY_CHECK_INTERVAL,
            last_memory_pressure: None,
            fine_timers: false,
            fine_timers_unwanted_since: None,
            preloading: true,
            next_preload_tick: t,
        }
    }

    /// Windows rounds every timed wait of a process that hasn't asked for a finer timer
    /// resolution up to its 15.6 ms tick, even when another program raised the system's (Windows
    /// 10 2004 and later). Servo paces frames with timed waits: the script thread's 20 ms
    /// rendering-update timer, the 8 ms refresh-driver timer, this loop's repaint pacing. At
    /// 15.6 ms granularity they land on 31 or 47 ms instead, so a 60 fps video was shown at about
    /// 32 frames a second, unevenly, and a 30 fps one alternated 16/31/47 ms frames: micro
    /// stutter. Hold a 1 ms resolution while a shown page animates, loads or keeps producing
    /// frames (a playing video: its frames are shown without the page animating), like Chromium
    /// and Firefox do while they need it, and let it go when idle to save power. Background
    /// tabs don't count: nothing of theirs is on screen. It is taken at once and given back only
    /// after it has been unneeded for `FINE_TIMER_RELEASE_DELAY` (a page between two animation
    /// frames looks idle for a moment; flipping on every such gap cost a system call each time).
    fn update_timer_resolution(&mut self) {
        // Also while the user interacts (the 750 ms grace after every input event,
        // `keep_awake_for`): input waits for the page's next rendering update, which the script
        // thread schedules with a 20 ms timer (script_thread.rs:1347-1366); at the default 15.6 ms
        // tick that is 31 ms, so hovering, typing and wheel-scrolling a static page answered a
        // frame or two later than they had to.
        let interacting = self.tick_until.is_some_and(|until| Instant::now() < until);
        let wanted = match &self.state {
            AppState::Running(state) => {
                interacting ||
                    state.showing_moving_pictures() ||
                    state.windows().iter().any(|window| {
                        window.active_webview().is_some_and(|webview| {
                            webview.animating() ||
                                !matches!(webview.load_status(), LoadStatus::Complete)
                        })
                    })
            },
            _ => false,
        };
        if wanted {
            self.fine_timers_unwanted_since = None;
        } else if self.fine_timers {
            let now = Instant::now();
            let since = *self.fine_timers_unwanted_since.get_or_insert(now);
            if now.duration_since(since) < FINE_TIMER_RELEASE_DELAY {
                return;
            }
            self.fine_timers_unwanted_since = None;
        }
        if wanted == self.fine_timers {
            return;
        }
        self.fine_timers = wanted;
        #[cfg(target_os = "windows")]
        // SAFETY: plain Win32 calls; every timeBeginPeriod is matched by one timeEndPeriod.
        unsafe {
            if wanted {
                timeBeginPeriod(1);
            } else {
                timeEndPeriod(1);
            }
        }
        diagnostics::log(format!("timer resolution: fine={wanted}"));
    }

    /// Initialize Application once event loop start running.
    pub fn init(&mut self, active_event_loop: Option<&ActiveEventLoop>) {
        let mut protocol_registry = ProtocolRegistry::default();
        let _ = protocol_registry.register(
            "urlinfo",
            protocols::urlinfo::UrlInfoProtocolHander::default(),
        );
        let _ =
            protocol_registry.register("servo", protocols::servo::ServoProtocolHandler::default());
        let _ = protocol_registry.register(
            "resource",
            protocols::resource::ResourceProtocolHandler::default(),
        );

        let servo_builder = ServoBuilder::default()
            .opts(self.opts.clone())
            .preferences(self.preferences.clone())
            .protocol_registry(protocol_registry)
            .event_loop_waker(self.waker.clone());

        let url = self.initial_url.as_url().clone();

        let servo = servo_builder.build();
        let platform_window = self.create_platform_window(url, active_event_loop);

        #[cfg(feature = "webxr")]
        servo.register_webxr_registry(super::webxr::XrDiscoveryWebXrRegistry::new_boxed(
            platform_window.clone(),
            active_event_loop,
            &self.preferences,
        ));

        servo.setup_logging();

        let user_content_manager = Rc::new(UserContentManager::new(&servo));
        // Stillwater's compatibility shims for web APIs Servo doesn't have yet (see compat.js).
        user_content_manager.add_script(Rc::new(UserScript::new(
            include_str!("compat.js").to_owned(),
            None,
        )));
        for script in load_userscripts(self.servoshell_preferences.userscripts_directory.as_deref())
            .expect("Loading userscripts failed")
        {
            user_content_manager.add_script(Rc::new(script));
        }

        for user_stylesheet in &self.servoshell_preferences.user_stylesheets {
            user_content_manager.add_stylesheet(user_stylesheet.clone());
        }

        let running_state = Rc::new(RunningAppState::new(
            servo,
            self.servoshell_preferences.clone(),
            self.waker.clone(),
            user_content_manager,
            self.preferences.clone(),
            #[cfg(feature = "gamepad")]
            self.event_loop_proxy
                .clone()
                .map(ServoshellGamepadDelegate::new)
                .map(Rc::new),
        ));
        let initial_url = self.initial_url.as_url().clone();
        running_state.open_window(
            platform_window,
            TopLevelWebViewCreationRequest::WithUrl(initial_url),
        );
        // The browser reopened this page by itself: its videos don't start until the user
        // interacts with it.
        if self.initial_url_restored {
            for window in running_state.windows().iter() {
                if let Some(webview) = window.active_webview() {
                    webview.block_autoplay_until_activation();
                }
            }
        }

        self.state = AppState::Running(running_state);
    }

    #[servo::servo_tracing::instrument(level = "debug", skip_all)]
    fn create_platform_window(
        &self,
        url: Url,
        active_event_loop: Option<&ActiveEventLoop>,
    ) -> Rc<dyn PlatformWindow> {
        assert_eq!(
            self.servoshell_preferences.headless,
            active_event_loop.is_none()
        );

        let Some(active_event_loop) = active_event_loop else {
            return HeadlessWindow::new(&self.servoshell_preferences);
        };

        HeadedWindow::new(
            &self.servoshell_preferences,
            active_event_loop,
            self.event_loop_proxy
                .clone()
                .expect("Should always have event loop proxy in headed mode."),
            url,
        )
    }

    /// Whether Servo has work in flight that needs the event loop to keep turning: a page
    /// is loading or animating, or we're within a short grace period after launch or input.
    ///
    /// servoshell used to stay awake as a side effect of egui's toolbar requesting repaints.
    /// Stillwater's toolbar is an HTML page, so without this the loop can fall asleep with
    /// frames pending and the window stays white until the mouse moves.
    fn busy(&self) -> bool {
        if self.tick_until.is_some_and(|until| Instant::now() < until) ||
            crate::desktop::capture::busy()
        {
            return true;
        }
        let AppState::Running(state) = &self.state else {
            return false;
        };
        state.windows().iter().any(|window| {
            window.webviews().iter().any(|(_, webview)| {
                webview.animating() || !matches!(webview.load_status(), LoadStatus::Complete)
            })
        })
    }

    /// Why the loop is (or isn't) busy, for the diagnostic log.
    fn busy_reason(&self) -> String {
        let grace = self
            .tick_until
            .map(|until| until.saturating_duration_since(Instant::now()).as_millis())
            .unwrap_or(0);
        let AppState::Running(state) = &self.state else {
            return format!("grace={grace}ms (not running)");
        };
        let mut loading = 0;
        let mut animating = 0;
        for window in state.windows().iter() {
            for (_, webview) in window.webviews() {
                if !matches!(webview.load_status(), LoadStatus::Complete) {
                    loading += 1;
                }
                if webview.animating() {
                    animating += 1;
                }
            }
        }
        format!("busy={} grace={grace}ms loading={loading} animating={animating}", self.busy())
    }

    /// Keep the loop alive for at least duration from now.
    fn keep_awake_for(&mut self, duration: std::time::Duration) {
        let until = Instant::now() + duration;
        if self.tick_until.is_none_or(|current| current < until) {
            self.tick_until = Some(until);
        }
    }

    fn control_flow(&self) -> ControlFlow {
        let flow = self.control_flow_without_repaint();
        // A repaint held back for a newly shown tab's first frame is due by then at the latest.
        let switch_wait = match &self.state {
            AppState::Running(state) => state
                .windows()
                .iter()
                .filter_map(|window| window.platform_window().as_headed_window()?.switch_wait_deadline())
                .min(),
            _ => None,
        };
        let repaint = match (self.repaint_at, switch_wait) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        match (flow, repaint) {
            (ControlFlow::WaitUntil(at), Some(repaint)) => ControlFlow::WaitUntil(at.min(repaint)),
            (flow, _) => flow,
        }
    }

    fn control_flow_without_repaint(&self) -> ControlFlow {
        if self.busy() {
            // ~60 Hz while busy; sleep when idle so an idle browser costs almost no CPU, waking
            // only to check for tabs to unload.
            ControlFlow::WaitUntil(Instant::now() + std::time::Duration::from_millis(16))
        } else {
            let mut wake = self.next_memory_check;
            if let AppState::Running(state) = &self.state &&
                let Some(gc_due) = state.idle_gc_due()
            {
                wake = wake.min(gc_due);
            }
            if self.fine_timers {
                wake = wake.min(Instant::now() + std::time::Duration::from_secs(1));
            }
            if self.preloading {
                wake = wake.min(self.next_preload_tick);
            }
            // A page scrollbar fading out (nothing once it has: see `scrollbar::next_wake`).
            if let Some(at) = crate::desktop::scrollbar::next_wake() {
                wake = wake.min(at);
            }
            ControlFlow::WaitUntil(wake)
        }
    }

    /// Start background loads of restored tabs when it's their turn (see
    /// `Sidebar::preload_tick`), at most every `PRELOAD_TICK_INTERVAL`. Called after every turn
    /// of the event loop: a page that animates keeps the loop from ever reaching a timed wake-up.
    fn maybe_preload(&mut self) {
        if !self.preloading || Instant::now() < self.next_preload_tick {
            return;
        }
        let AppState::Running(state) = &self.state else {
            return;
        };
        self.next_preload_tick = Instant::now() + PRELOAD_TICK_INTERVAL;
        let mut more = false;
        for window in state.windows().iter() {
            if let Some(headed_window) = window.platform_window().as_headed_window() {
                more |= headed_window.preload_tick(window);
            }
        }
        self.preloading = more;
    }

    /// Unload tabs that haven't been used for a while (see the sidebar's settings) and, when
    /// Windows is low on memory, the least recently used background tab.
    fn check_memory(&mut self) {
        let low_memory = crate::desktop::memory::system_memory_low();
        self.next_memory_check = Instant::now() +
            if low_memory {
                LOW_MEMORY_CHECK_INTERVAL
            } else {
                MEMORY_CHECK_INTERVAL
            };
        let AppState::Running(state) = &self.state else {
            return;
        };
        let playing = state.playing_media();
        let mut unloaded = 0;
        for window in state.windows().iter() {
            if let Some(headed_window) = window.platform_window().as_headed_window() {
                unloaded += headed_window.unload_inactive_tabs(window, &playing, low_memory);
            }
        }
        if unloaded > 0 {
            diagnostics::log(format!(
                "memory check: low_memory={low_memory} unloaded={unloaded}"
            ));
        }
        // Also drop rendering caches and collect garbage, at most once a minute.
        if low_memory &&
            self.last_memory_pressure
                .is_none_or(|last| last.elapsed() >= std::time::Duration::from_secs(60))
        {
            self.last_memory_pressure = Some(Instant::now());
            diagnostics::log("memory is low: asking Servo to free caches");
            state.servo().notify_memory_pressure();
        }
    }

    pub fn pump_servo_event_loop(&mut self, active_event_loop: Option<&ActiveEventLoop>) -> bool {
        let AppState::Running(state) = &self.state else {
            return false;
        };

        let create_platform_window = |url: Url| self.create_platform_window(url, active_event_loop);
        if !state.spin_event_loop(Some(&create_platform_window)) {
            // Usually armed already, when the last window was closed. Tabs, history, passwords
            // and settings are saved as they change, so nothing is lost if the engine is slow
            // to stop (it waits for every page's script thread, and a busy page can hold that
            // up): the watchdog ends the process after a few seconds. It must be armed before
            // the next line, which drops the last reference to Servo and so blocks until the
            // engine has stopped.
            crate::desktop::shutdown::begin("event loop stopping");
            diagnostics::log("shutdown: stopping the engine");
            self.state = AppState::ShuttingDown;
            diagnostics::log("shutdown: engine stopped");
            return false;
        }

        // Don't let a busy event queue starve painting (see HeadedWindow::redraw_if_pending).
        let windows: Vec<_> = state.windows().iter().cloned().collect();
        for window in windows {
            if let Some(headed_window) = window.platform_window().as_headed_window() {
                headed_window.redraw_if_pending(state.clone(), window.clone());
            }
        }
        true
    }
}

impl ApplicationHandler<AppEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        diagnostics::log("resumed: initialising Servo");
        self.init(Some(event_loop));
        diagnostics::log("resumed: Servo initialised");
        // Startup creates the window, the sidebar and the first page: stay awake for it.
        self.keep_awake_for(std::time::Duration::from_secs(5));
        event_loop.set_control_flow(self.control_flow());
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        self.update_timer_resolution();
        if self.preloading {
            self.maybe_preload();
            // Wake up for the next look (see `control_flow_without_repaint`).
            event_loop.set_control_flow(self.control_flow());
        }
    }

    fn new_events(&mut self, event_loop: &ActiveEventLoop, cause: winit::event::StartCause) {
        if !matches!(cause, winit::event::StartCause::ResumeTimeReached { .. }) {
            return;
        }
        diagnostics::count("tick");
        diagnostics::maybe_summarize(|| self.busy_reason());
        if self.repaint_at.is_some_and(|at| Instant::now() >= at) {
            // A held-back repaint: this turn paints it (windows were marked by its request).
            self.repaint_at = None;
            self.last_repaint_request = Some(Instant::now());
        }
        if Instant::now() >= self.next_memory_check {
            self.check_memory();
        }
        if let AppState::Running(state) = &self.state {
            state.maybe_collect_garbage();
        }
        self.maybe_preload();

        if let AppState::Running(state) = &self.state {
            // "Copying GIF…" / "GIF copied" in the status line.
            let copy_notice_changed = crate::desktop::capture::take_changed();
            for window in state.windows().iter() {
                // A safety net for frames nobody asked to show (the window staying white until
                // the mouse moves), not a frame clock: while the loop is busy it ticks every
                // 16 ms, and repainting on each tick redrew the whole window (page, sidebar,
                // chrome) about 60 times a second during any page load, animation or the
                // 750 ms after every input event, even when nothing had changed. Real changes
                // already ask for a repaint (new frames, egui, the sidebar's waker).
                let painted_recently = window
                    .platform_window()
                    .as_headed_window()
                    .is_some_and(|headed| headed.painted_within(std::time::Duration::from_millis(250)));
                if !painted_recently || crate::desktop::scrollbar::wake_due() {
                    window.set_needs_repaint();
                }
                if copy_notice_changed {
                    window.set_needs_update();
                }
            }
        }
        if !self.pump_servo_event_loop(event_loop.into()) {
            event_loop.exit();
        }
        event_loop.set_control_flow(self.control_flow());
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
        window_event: WindowEvent,
    ) {
        diagnostics::count(window_event_name(&window_event));
        if let WindowEvent::MouseInput { state, button, .. } = &window_event {
            diagnostics::log(format!("mouse {button:?} {state:?}"));
        }
        if let WindowEvent::Focused(focused) = &window_event {
            diagnostics::log(format!("window focused={focused}"));
        }
        diagnostics::maybe_summarize(|| self.busy_reason());
        // Input can start work (navigation, scrolling, sidebar changes) that needs a few frames.
        if !matches!(window_event, WindowEvent::RedrawRequested) {
            self.keep_awake_for(std::time::Duration::from_millis(750));
        }
        let now = Instant::now();
        trace_winit_event!(
            window_event,
            "@{:?} (+{:?}) {window_event:?}",
            now - self.t_start,
            now - self.t
        );
        self.t = now;

        let AppState::Running(state) = &self.state else {
            return;
        };

        if let Some(window) = state.window(ServoShellWindowId::from(u64::from(window_id))) &&
            let Some(headed_window) = window.platform_window().as_headed_window()
        {
            headed_window.handle_winit_window_event(state.clone(), window, window_event);
        }

        if !self.pump_servo_event_loop(event_loop.into()) {
            event_loop.exit();
        }
        // Block until the window gets an event
        event_loop.set_control_flow(self.control_flow());
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, app_event: AppEvent) {
        // Also here, not only in `about_to_wait`: a loop that never runs dry (frames arriving
        // faster than the window can show them) never gets there.
        self.update_timer_resolution();
        self.maybe_preload();
        let AppState::Running(state) = &self.state else {
            return;
        };

        diagnostics::count(match &app_event {
            AppEvent::Waker => "app_event:waker",
            AppEvent::RequestRepaint(..) => "app_event:repaint",
            AppEvent::Translate(..) => "app_event:translate",
            _ => "app_event:other",
        });
        // A flood of events would otherwise never show in the summary (it's written on ticks
        // and window events).
        diagnostics::maybe_summarize(|| self.busy_reason());
        if !matches!(app_event, AppEvent::Waker | AppEvent::RequestRepaint(..)) {
            diagnostics::log(format!("app event: {app_event:?}"));
        }
        let opened_url = matches!(app_event, AppEvent::OpenUrl(..));
        // Repaint requests coming faster than the monitor shows frames mean something is asking
        // to repaint after every repaint. Holding the repaint until the next frame of the window's
        // monitor (60, 144, 240 Hz...) paces it like vsync, so nothing real gets slower, and
        // leaves the thread time for its mouse and keyboard input, which Windows only delivers
        // when no posted events are waiting: a runaway repaint loop must never be able to hold
        // the mouse and lock up the desktop.
        let mut defer_repaint = false;
        // A wake carrying a sidebar command (a tab clicked, Ctrl+Tab...) is user input: act on
        // it at once (`update_user_interface_state`); only its repaint keeps to the pacing.
        let sidebar_commands = match &app_event {
            AppEvent::RequestRepaint(window_id, _) => state.window(*window_id).is_some_and(|window| {
                window
                    .platform_window()
                    .as_headed_window()
                    .is_some_and(|headed| headed.has_pending_sidebar_commands())
            }),
            _ => false,
        };
        if let AppEvent::RequestRepaint(window_id, _) = &app_event &&
            !sidebar_commands
        {
            let now = Instant::now();
            let frame = state
                .window(*window_id)
                .and_then(|window| window.platform_window().as_headed_window().map(|headed| headed.frame_interval()))
                .unwrap_or(std::time::Duration::from_micros(16_667));
            if let Some(last) = self.last_repaint_request &&
                now.duration_since(last) < frame
            {
                defer_repaint = true;
                let due = last + frame;
                self.repaint_at = Some(self.repaint_at.map_or(due, |at| at.min(due)));
            } else {
                self.last_repaint_request = Some(now);
            }
        }
        match app_event {
            // Cleared before pumping Servo, so a wake during the pump queues a new one.
            AppEvent::Waker => {
                crate::desktop::event_loop::WAKER_QUEUED.store(false, std::sync::atomic::Ordering::Release)
            },
            AppEvent::Accessibility(ref event) => {
                if let Some(window) =
                    state.window(ServoShellWindowId::from(u64::from(event.window_id))) &&
                    let Some(headed_window) = window.platform_window().as_headed_window()
                {
                    headed_window.handle_winit_app_event(state.clone(), app_event);
                }
            },
            AppEvent::RequestRepaint(window_id, queued) => {
                queued.store(false, std::sync::atomic::Ordering::Release);
                if let Some(window) = state.window(window_id) {
                    window.set_needs_repaint();
                    if sidebar_commands {
                        window.set_needs_update();
                    }
                }
            },
            // A launch that gave up waiting for us has opened its own window: don't open it twice.
            AppEvent::OpenUrl(_, Some(ref handoff)) if !handoff.claim() => {},
            AppEvent::OpenUrl(input, _) => {
                let window = state
                    .focused_window()
                    .or_else(|| state.windows().first().cloned());
                if let Some(window) = window {
                    let url = crate::parser::location_bar_input_to_url(
                        &input,
                        &state.servoshell_preferences.searchpage,
                    );
                    if let Some(url) = url.filter(|_| !input.is_empty()) {
                        window.create_and_activate_toplevel_webview(
                            state.clone(),
                            TopLevelWebViewCreationRequest::WithUrl(url.into_url()),
                        );
                    }
                    if let Some(headed_window) = window.platform_window().as_headed_window()
                    {
                        headed_window.bring_to_front();
                    }
                }
            },
            AppEvent::UpdateAvailable(update) => {
                for window in state.windows().iter() {
                    if let Some(headed_window) = window.platform_window().as_headed_window() {
                        headed_window.with_sidebar(|sidebar| sidebar.set_update_available(update.clone()));
                    }
                    window.set_needs_update();
                }
            },
            AppEvent::UpdateStatus(status) => {
                for window in state.windows().iter() {
                    if let Some(headed_window) = window.platform_window().as_headed_window() {
                        headed_window.with_sidebar(|sidebar| sidebar.set_update_status(status.clone()));
                    }
                    window.set_needs_update();
                }
            },
            AppEvent::UpdateReadyToRestart => state.schedule_exit(),
            AppEvent::Compare(event) => {
                use crate::desktop::compare::CompareEvent;
                let window = state
                    .focused_window()
                    .or_else(|| state.windows().first().cloned());
                if let Some(window) = window {
                    let status = match &event {
                        CompareEvent::Status(message) => serde_json::json!({ "busy": true, "message": message }),
                        CompareEvent::Done(_) => serde_json::json!({ "busy": false, "message": "The comparison opened in a new tab." }),
                        CompareEvent::Failed(message) => serde_json::json!({ "busy": false, "message": message, "error": true }),
                    };
                    if let Some(headed_window) = window.platform_window().as_headed_window() {
                        headed_window.with_sidebar(|sidebar| sidebar.set_compare(status));
                    }
                    // A new tab, without bringing the window forward: the user may have switched
                    // to something else while Edge loaded the page.
                    if let CompareEvent::Done(url) = event {
                        window.create_and_activate_toplevel_webview(
                            state.clone(),
                            TopLevelWebViewCreationRequest::WithUrl(url),
                        );
                    }
                    window.set_needs_update();
                }
            },
            AppEvent::Translate(event) => {
                state
                    .translation
                    .borrow_mut()
                    .handle(event, |id| state.webview_by_id(id));
                for window in state.windows().iter() {
                    window.set_needs_update();
                }
            },
            AppEvent::UpdateCheckDone(message) => {
                for window in state.windows().iter() {
                    if let Some(headed_window) = window.platform_window().as_headed_window() {
                        headed_window.with_sidebar(|sidebar| sidebar.set_update_check(message.clone()));
                    }
                    window.set_needs_update();
                }
            },
            #[cfg(feature = "gamepad")]
            AppEvent::Gamepad(event, gamepad_name, gamepad_index) => {
                state.handle_gamepad_events(event, gamepad_name, gamepad_index);
            },
        }

        if opened_url {
            self.keep_awake_for(std::time::Duration::from_secs(3));
        }
        if defer_repaint {
            // The loop wakes at `repaint_at` (see `control_flow`) and paints then.
            event_loop.set_control_flow(self.control_flow());
            return;
        }
        if !self.pump_servo_event_loop(event_loop.into()) {
            event_loop.exit();
        }

        // Block until the window gets an event
        event_loop.set_control_flow(self.control_flow());
    }
}

fn load_userscripts(userscripts_directory: Option<&Path>) -> std::io::Result<Vec<UserScript>> {
    let mut userscripts = Vec::new();
    if let Some(userscripts_directory) = &userscripts_directory {
        let mut files = std::fs::read_dir(userscripts_directory)?
            .map(|e| e.map(|entry| entry.path()))
            .collect::<Result<Vec<_>, _>>()?;
        files.sort_unstable();
        for file in files {
            let script = std::fs::read_to_string(&file)?;
            userscripts.push(UserScript::new(script, Some(file)));
        }
    }
    Ok(userscripts)
}


fn window_event_name(event: &WindowEvent) -> &'static str {
    match event {
        WindowEvent::RedrawRequested => "redraw",
        WindowEvent::CursorMoved { .. } => "cursor_moved",
        WindowEvent::CursorEntered { .. } => "cursor_entered",
        WindowEvent::CursorLeft { .. } => "cursor_left",
        WindowEvent::MouseInput { .. } => "mouse_input",
        WindowEvent::MouseWheel { .. } => "mouse_wheel",
        WindowEvent::KeyboardInput { .. } => "keyboard",
        WindowEvent::ModifiersChanged(..) => "modifiers",
        WindowEvent::Focused(..) => "focused",
        WindowEvent::Resized(..) => "resized",
        WindowEvent::Moved(..) => "moved",
        WindowEvent::Occluded(..) => "occluded",
        WindowEvent::ScaleFactorChanged { .. } => "scale_factor",
        WindowEvent::Ime(..) => "ime",
        WindowEvent::CloseRequested => "close_requested",
        _ => "other_window_event",
    }
}