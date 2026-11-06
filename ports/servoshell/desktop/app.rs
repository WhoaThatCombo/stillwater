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
    t_start: Instant,
    t: Instant,
    state: AppState,
    /// Keep the event loop ticking until this time, e.g. after another launch handed us a URL:
    /// on Windows the window otherwise may not repaint until it receives its next message.
    tick_until: Option<Instant>,
}

impl App {
    pub fn new(
        opts: Opts,
        preferences: Preferences,
        servo_shell_preferences: ServoShellPreferences,
        event_loop: &ServoShellEventLoop,
    ) -> Self {
        let initial_url = get_default_url(
            servo_shell_preferences.url.as_deref(),
            env::current_dir().unwrap(),
            |path| fs::metadata(path).is_ok(),
            &servo_shell_preferences,
        );

        let t = Instant::now();
        App {
            opts,
            preferences,
            servoshell_preferences: servo_shell_preferences,
            waker: event_loop.create_event_loop_waker(),
            event_loop_proxy: event_loop.event_loop_proxy(),
            initial_url,
            t_start: t,
            t,
            state: AppState::Initializing,
            tick_until: None,
        }
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
        if self.tick_until.is_some_and(|until| Instant::now() < until) {
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
        if self.busy() {
            // ~60 Hz while busy; sleep fully when idle so an idle browser costs no CPU.
            ControlFlow::WaitUntil(Instant::now() + std::time::Duration::from_millis(16))
        } else {
            ControlFlow::Wait
        }
    }

    pub fn pump_servo_event_loop(&mut self, active_event_loop: Option<&ActiveEventLoop>) -> bool {
        let AppState::Running(state) = &self.state else {
            return false;
        };

        let create_platform_window = |url: Url| self.create_platform_window(url, active_event_loop);
        if !state.spin_event_loop(Some(&create_platform_window)) {
            self.state = AppState::ShuttingDown;
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

    fn new_events(&mut self, event_loop: &ActiveEventLoop, cause: winit::event::StartCause) {
        if !matches!(cause, winit::event::StartCause::ResumeTimeReached { .. }) {
            return;
        }
        diagnostics::count("tick");
        diagnostics::maybe_summarize(|| self.busy_reason());
        if let AppState::Running(state) = &self.state {
            for window in state.windows().iter() {
                window.set_needs_repaint();
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
        let AppState::Running(state) = &self.state else {
            return;
        };

        diagnostics::count("app_event");
        if !matches!(app_event, AppEvent::Waker | AppEvent::RequestRepaint(_)) {
            diagnostics::log(format!("app event: {app_event:?}"));
        }
        let opened_url = matches!(app_event, AppEvent::OpenUrl(_));
        match app_event {
            AppEvent::Waker => (),
            AppEvent::Accessibility(ref event) => {
                if let Some(window) =
                    state.window(ServoShellWindowId::from(u64::from(event.window_id))) &&
                    let Some(headed_window) = window.platform_window().as_headed_window()
                {
                    headed_window.handle_winit_app_event(state.clone(), app_event);
                }
            },
            AppEvent::RequestRepaint(window_id) => {
                if let Some(window) = state.window(window_id) {
                    window.set_needs_repaint();
                }
            },
            AppEvent::OpenUrl(input) => {
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
            #[cfg(feature = "gamepad")]
            AppEvent::Gamepad(event, gamepad_name, gamepad_index) => {
                state.handle_gamepad_events(event, gamepad_name, gamepad_index);
            },
        }

        if opened_url {
            self.keep_awake_for(std::time::Duration::from_secs(3));
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