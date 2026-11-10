/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Shared state and methods for desktop and EGL implementations.

use std::cell::{Cell, Ref, RefCell};
use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use crossbeam_channel::{Receiver, Sender, unbounded};
use euclid::Rect;
#[cfg(all(
    feature = "gamepad",
    not(any(target_os = "android", target_env = "ohos"))
))]
use gilrs::Event;
use image::{DynamicImage, ImageFormat, RgbaImage};
#[cfg(all(
    any(coverage, llvm_pgo),
    any(target_os = "android", target_env = "ohos")
))]
use libc::c_char;
use log::{error, info, warn};
#[cfg(all(
    feature = "gamepad",
    not(any(target_os = "android", target_env = "ohos"))
))]
use servo::GamepadIndex;
use servo::{
    AllowOrDenyRequest, AuthenticationRequest, BluetoothDeviceSelectionRequest, CSSPixel,
    ConsoleLogLevel, CreateNewWebViewRequest, DeviceIntPoint, DeviceIntSize, EmbedderControl,
    EmbedderControlId, EventLoopWaker, GenericSender, InputEvent, InputEventId, InputEventResult,
    JSValue, LoadStatus, PermissionRequest, PrefValue, Preferences, ScreenshotCaptureError, Servo,
    ServoDelegate, ServoError, TraversalId, UserContentManager, WebDriverCommandMsg,
    WebDriverJSResult, WebDriverLoadStatus, WebDriverScriptCommand, WebDriverSenders, WebView,
    WebViewDelegate, WebViewId,
};
use url::Url;

#[cfg(all(
    feature = "gamepad",
    not(any(target_os = "android", target_env = "ohos"))
))]
pub(crate) use crate::desktop::gamepad::ServoshellGamepadDelegate;
use crate::prefs::{EXPERIMENTAL_PREFS, ServoShellPreferences};
use crate::webdriver::WebDriverEmbedderControls;
use crate::window::{
    PlatformWindow, ServoShellWindow, ServoShellWindowId, TopLevelWebViewCreationRequest,
};

#[cfg(all(
    any(coverage, llvm_pgo),
    any(target_os = "android", target_env = "ohos")
))]
unsafe extern "C" {
    fn __llvm_profile_set_filename(file: *const c_char);
    fn __llvm_profile_write_file();
}

pub struct WebViewCollection {
    /// List of top-level browsing contexts.
    /// Modified by EmbedderMsg::WebViewOpened and EmbedderMsg::WebViewClosed,
    /// and we exit if it ever becomes empty.
    webviews: HashMap<WebViewId, WebView>,

    /// The order in which the webviews were created.
    pub(crate) creation_order: Vec<WebViewId>,

    /// The [`WebView`] that is currently active. This is the [`WebView`] that is shown and has
    /// input focus.
    active_webview_id: Option<WebViewId>,

    /// Whether or not the active WebView in this collection has system focus.
    has_system_focus: bool,
}

impl Default for WebViewCollection {
    fn default() -> Self {
        Self {
            webviews: Default::default(),
            creation_order: Default::default(),
            active_webview_id: None,
            // A real window learns it has focus when it is first shown. Automated tests' windows
            // never take focus (`STILLWATER_BACKGROUND_WINDOW`), so their pages count as focused
            // until told otherwise, as they always have.
            has_system_focus: std::env::var_os("STILLWATER_BACKGROUND_WINDOW").is_some(),
        }
    }
}

impl WebViewCollection {
    pub fn add(&mut self, webview: WebView) {
        let id = webview.id();
        self.creation_order.push(id);
        self.webviews.insert(id, webview);
    }

    /// Removes a webview from the collection by [`WebViewId`]. If the removed [`WebView`] was the active
    /// [`WebView`] then the next newest [`WebView`] will be activated.
    pub fn remove(&mut self, id: WebViewId) -> Option<WebView> {
        self.creation_order.retain(|&webview_id| webview_id != id);
        let removed_webview = self.webviews.remove(&id);

        if self.active_webview_id == Some(id) {
            self.active_webview_id = None;
            if let Some(newest) = self.creation_order.last() {
                self.activate_webview(*newest);
            }
        }

        removed_webview
    }

    pub fn get(&self, id: WebViewId) -> Option<&WebView> {
        self.webviews.get(&id)
    }

    pub fn active(&self) -> Option<&WebView> {
        self.active_webview_id.and_then(|id| self.webviews.get(&id))
    }

    pub fn active_id(&self) -> Option<WebViewId> {
        self.active_webview_id
    }

    /// Gets a reference to the most recently created webview, if any.
    pub fn newest(&self) -> Option<&WebView> {
        self.creation_order
            .last()
            .and_then(|id| self.webviews.get(id))
    }

    pub fn all_in_creation_order(&self) -> impl Iterator<Item = (WebViewId, &WebView)> {
        self.creation_order
            .iter()
            .filter_map(move |id| self.webviews.get(id).map(|webview| (*id, webview)))
    }

    /// Returns an iterator over all webview references (in arbitrary order).
    pub fn values(&self) -> impl Iterator<Item = &WebView> {
        self.webviews.values()
    }

    /// Returns true if the collection contains no webviews.
    pub fn is_empty(&self) -> bool {
        self.webviews.is_empty()
    }

    pub(crate) fn activate_webview(&mut self, id_to_activate: WebViewId) {
        assert!(self.creation_order.contains(&id_to_activate));

        self.active_webview_id = Some(id_to_activate);
        for (webview_id, webview) in self.all_in_creation_order() {
            // WebViews start out "focused", so set this both ways: a tab opened or switched to
            // while Stillwater is in the background must paint its selection inactive (grey)
            // and must not show a caret, like Chrome and Firefox.
            if id_to_activate == webview_id {
                webview.set_focused(self.has_system_focus);
                webview.show();
            } else {
                webview.set_focused(false);
                webview.hide();
            }
        }
    }

    pub(crate) fn activate_webview_by_index(&mut self, index: usize) {
        let Some(webview_id) = self.creation_order.get(index) else {
            // Just ignore requests to activate unknown WebViews. This can happen by pressing
            // keyboard shortcuts in the interface.
            return;
        };
        self.activate_webview(*webview_id);
    }

    /// Whether the window holding these WebViews is the active window.
    pub(crate) fn has_system_focus(&self) -> bool {
        self.has_system_focus
    }

    pub(crate) fn set_focused(&mut self, focused: bool) {
        self.has_system_focus = focused;
        if let Some(active) = self.active() {
            active.set_focused(focused);
        }
    }
}

/// A command received via the user interacting with the user interface.
#[cfg_attr(any(target_os = "android", target_env = "ohos"), expect(dead_code))]
pub(crate) enum UserInterfaceCommand {
    Go(String),
    Back,
    Forward,
    Reload,
    ReloadAll,
    NewWebView,
    /// Open a new tab at the given URL (e.g. loading a pinned tab).
    NewWebViewWithUrl(Url),
    /// A new tab that loads in the background (the page context menu's "Open link in
    /// background tab").
    NewBackgroundWebViewWithUrl(Url),
    CloseWebView(WebViewId),
    NewWindow(TopLevelWebViewCreationRequest),
}

pub(crate) struct RunningAppState {
    /// The gamepad provider, used for handling gamepad events and set on each WebView.
    /// May be `None` if gamepad support is disabled or failed to initialize.
    #[cfg(all(
        feature = "gamepad",
        not(any(target_os = "android", target_env = "ohos"))
    ))]
    gamepad_delegate: Option<Rc<ServoshellGamepadDelegate>>,

    /// The [`WebDriverSenders`] used to reply to pending WebDriver requests.
    pub(crate) webdriver_senders: RefCell<WebDriverSenders>,

    /// When running in WebDriver mode, [`WebDriverEmbedderControls`] is a virtual container
    /// for all embedder controls. This overrides the normal behavior where these controls
    /// are shown in the GUI or not processed at all in headless mode.
    pub(crate) webdriver_embedder_controls: WebDriverEmbedderControls,

    /// A [`HashMap`] of pending WebDriver events. It is the WebDriver embedder's responsibility
    /// to inform the WebDriver server when the event has been fully handled. This map is used
    /// to report back to WebDriver when that happens.
    pub(crate) pending_webdriver_events: RefCell<HashMap<InputEventId, (WebViewId, Sender<()>)>>,

    /// A [`Receiver`] for receiving commands from a running WebDriver server, if WebDriver
    /// was enabled.
    pub(crate) webdriver_receiver: Option<Receiver<WebDriverCommandMsg>>,

    /// servoshell specific preferences created during startup of the application.
    pub(crate) servoshell_preferences: ServoShellPreferences,

    /// A handle to the Servo instance.
    pub(crate) servo: Servo,

    /// Whether or not the application has achieved stable image output. This is used
    /// for the `exit_after_stable_image` option.
    pub(crate) achieved_stable_image: Rc<Cell<bool>>,

    /// The [`UserContentManager`] for all `WebView`s created.
    pub(crate) user_content_manager: Rc<UserContentManager>,
    /// Each tab's most recent script errors and warnings, for broken-site reports.
    recent_console_errors: RefCell<HashMap<WebViewId, std::collections::VecDeque<String>>>,
    /// The built-in content blocker.
    #[cfg(not(any(target_os = "android", target_env = "ohos")))]
    pub(crate) adblock: RefCell<crate::desktop::adblock::Adblock>,
    /// Saved passwords.
    #[cfg(not(any(target_os = "android", target_env = "ohos")))]
    pub(crate) passwords: RefCell<crate::desktop::passwords::Passwords>,
    /// Browsing history and bookmarks.
    #[cfg(not(any(target_os = "android", target_env = "ohos")))]
    pub(crate) history: RefCell<crate::desktop::history::History>,
    /// Downloads and the Downloads panel's list.
    #[cfg(not(any(target_os = "android", target_env = "ohos")))]
    pub(crate) downloads: crate::desktop::downloads::SharedDownloads,
    /// Page translation.
    #[cfg(not(any(target_os = "android", target_env = "ohos")))]
    pub(crate) translation: RefCell<crate::desktop::translate::Translations>,
    /// The colour scheme last sent as a client hint ("light" or "dark").
    color_scheme_hint: Cell<Option<&'static str>>,
    /// Tabs with audio or video playing, which the tab unloader leaves alone.
    playing_media: RefCell<HashMap<WebViewId, std::time::Instant>>,
    /// When to run an idle garbage collection: a few seconds after pages finish loading or
    /// close, once things have settled.
    gc_due: Cell<Option<std::time::Instant>>,
    /// Page loads in progress (any tab), from their `LoadStatus::Started`: the idle garbage
    /// collection waits for them, since it stalls frames for tens of milliseconds.
    loads_in_progress: RefCell<HashMap<WebViewId, std::time::Instant>>,
    /// Whether a page was replaced or a tab closed since the renderer's caches were last dropped.
    render_caches_stale: Cell<bool>,

    /// Whether or not program exit has been triggered. This means that all windows
    /// will be destroyed and shutdown will start at the end of the current event loop.
    exit_scheduled: Cell<bool>,

    /// A screenshot deferred by `STILLWATER_SCREENSHOT_DELAY_MS`: when to take it, and of what.
    deferred_screenshot: RefCell<Option<(std::time::Instant, WebView)>>,

    /// Whether the user has enabled experimental preferences.
    experimental_preferences_enabled: Cell<bool>,

    /// The set of [`ServoShellWindow`]s that currently exist for this instance of servoshell.
    ///
    /// These are stored in creation order to ensure consistent focus behavior.
    // This is the last field of the struct to ensure that windows are dropped *after* all
    // other references to the relevant rendering contexts have been destroyed.
    // See https://github.com/servo/servo/issues/36711.
    windows: RefCell<Vec<Rc<ServoShellWindow>>>,

    /// The currently focused [`ServoShellWindow`], if one is focused.
    focused_window: RefCell<Option<Rc<ServoShellWindow>>>,

    /// Whether accessibility is active in servoshell.
    ///
    /// Set by the platform via AccessKit, and forwarded to existing and new WebViews via
    /// [`WebView::set_accessibility_active()`], in [`Self::set_accessibility_active()`] and
    /// and [`ServoShellWindow::create_toplevel_webview()`].
    accessibility_active: Cell<bool>,
}

impl RunningAppState {
    pub(crate) fn new(
        servo: Servo,
        servoshell_preferences: ServoShellPreferences,
        event_loop_waker: Box<dyn EventLoopWaker>,
        user_content_manager: Rc<UserContentManager>,
        default_preferences: Preferences,
        #[cfg(all(
            feature = "gamepad",
            not(any(target_os = "android", target_env = "ohos"))
        ))]
        gamepad_delegate: Option<Rc<ServoshellGamepadDelegate>>,
    ) -> Self {
        #[cfg(not(any(target_os = "android", target_env = "ohos")))]
        let downloads = crate::desktop::downloads::Downloads::load();
        servo.set_delegate(Rc::new(ServoShellServoDelegate {
            #[cfg(not(any(target_os = "android", target_env = "ohos")))]
            downloads: downloads.clone(),
        }));

        let webdriver_receiver = servoshell_preferences.webdriver_port.get().map(|port| {
            let (embedder_sender, embedder_receiver) = unbounded();
            webdriver_server::start_server(
                port,
                embedder_sender,
                event_loop_waker,
                default_preferences,
            );
            embedder_receiver
        });

        let experimental_preferences_enabled =
            Cell::new(servoshell_preferences.experimental_preferences_enabled);

        Self {
            windows: Default::default(),
            focused_window: Default::default(),
            #[cfg(all(
                feature = "gamepad",
                not(any(target_os = "android", target_env = "ohos"))
            ))]
            gamepad_delegate,
            webdriver_senders: RefCell::default(),
            webdriver_embedder_controls: Default::default(),
            pending_webdriver_events: Default::default(),
            webdriver_receiver,
            servoshell_preferences,
            servo,
            achieved_stable_image: Default::default(),
            exit_scheduled: Default::default(),
            deferred_screenshot: Default::default(),
            #[cfg(not(any(target_os = "android", target_env = "ohos")))]
            adblock: RefCell::new(crate::desktop::adblock::Adblock::new(
                user_content_manager.clone(),
            )),
            #[cfg(not(any(target_os = "android", target_env = "ohos")))]
            passwords: RefCell::new(crate::desktop::passwords::Passwords::new(
                &user_content_manager,
            )),
            #[cfg(not(any(target_os = "android", target_env = "ohos")))]
            history: RefCell::new(crate::desktop::history::History::new()),
            downloads,
            #[cfg(not(any(target_os = "android", target_env = "ohos")))]
            translation: RefCell::new(crate::desktop::translate::Translations::new()),
            user_content_manager,
            recent_console_errors: Default::default(),
            playing_media: Default::default(),
            color_scheme_hint: Cell::new(None),
            gc_due: Default::default(),
            loads_in_progress: Default::default(),
            render_caches_stale: Default::default(),
            experimental_preferences_enabled,
            accessibility_active: Cell::new(false),
        }
    }

    pub(crate) fn open_window(
        self: &Rc<Self>,
        platform_window: Rc<dyn PlatformWindow>,
        creation_request: TopLevelWebViewCreationRequest,
    ) -> Rc<ServoShellWindow> {
        let window = Rc::new(ServoShellWindow::new(platform_window.clone()));
        self.windows.borrow_mut().push(window.clone());
        window.create_and_activate_toplevel_webview(self.clone(), creation_request);

        // If the window already has platform focus, mark it as focused in our application state.
        if platform_window.has_platform_focus() {
            self.set_window_has_focus(window.clone(), true);
        }

        window
    }

    pub(crate) fn windows<'a>(&'a self) -> Ref<'a, Vec<Rc<ServoShellWindow>>> {
        self.windows.borrow()
    }

    pub(crate) fn focused_window(&self) -> Option<Rc<ServoShellWindow>> {
        self.focused_window.borrow().clone()
    }

    pub(crate) fn set_window_has_focus(&self, window: Rc<ServoShellWindow>, has_focus: bool) {
        let mut focused_window = self.focused_window.borrow_mut();

        // If another window had focus before and this new window is gaining it, then
        // ensure that the old window is explicitly unfocused.
        if let Some(previously_focused_window) = &*focused_window &&
            !Rc::ptr_eq(previously_focused_window, &window) &&
            has_focus
        {
            previously_focused_window
                .webview_collection
                .borrow_mut()
                .set_focused(false);
        }

        window
            .webview_collection
            .borrow_mut()
            .set_focused(has_focus);

        if let Some(previously_focused_window) = &*focused_window &&
            Rc::ptr_eq(previously_focused_window, &window) &&
            !has_focus
        {
            *focused_window = None;
        } else if has_focus {
            *focused_window = Some(window);
        }
    }

    #[cfg_attr(target_os = "android", expect(dead_code))]
    pub(crate) fn window(&self, id: ServoShellWindowId) -> Option<Rc<ServoShellWindow>> {
        self.windows
            .borrow()
            .iter()
            .find(|window| window.id() == id)
            .cloned()
    }

    pub(crate) fn webview_by_id(&self, webview_id: WebViewId) -> Option<WebView> {
        self.windows()
            .iter()
            .find_map(|window| window.webview_by_id(webview_id))
    }

    pub(crate) fn webdriver_receiver(&self) -> Option<&Receiver<WebDriverCommandMsg>> {
        self.webdriver_receiver.as_ref()
    }

    pub(crate) fn servo(&self) -> &Servo {
        &self.servo
    }

    #[cfg(all(
        feature = "gamepad",
        not(any(target_os = "android", target_env = "ohos"))
    ))]
    pub(crate) fn gamepad_delegate(&self) -> Option<Rc<ServoshellGamepadDelegate>> {
        self.gamepad_delegate.clone()
    }

    pub(crate) fn schedule_exit(&self) {
        // When explicitly required to shutdown, unset webdriver port
        // which allows normal shutdown.
        // Note that when not explicitly required to shutdown, we still keep Servo alive
        // when all tabs are closed when `webdriver_port` enabled, which is necessary
        // to run wpt test using servodriver.
        self.servoshell_preferences.webdriver_port.set(None);
        self.exit_scheduled.set(true);
        note_shutdown("exit requested");

        #[cfg(all(
            any(coverage, llvm_pgo),
            any(target_os = "android", target_env = "ohos")
        ))]
        {
            use std::ffi::CString;

            use crate::prefs::default_config_dir;

            let mut profile_path = default_config_dir().expect("Need a config dir");
            profile_path.push("profiles/");
            let filename = format!(
                "{}/profile-%h-%p.profraw",
                profile_path.to_str().expect("Should be unicode")
            );
            let c_filename = CString::new(filename).expect("Need a valid cstring");
            unsafe {
                __llvm_profile_set_filename(c_filename.as_ptr() as *const c_char);
                __llvm_profile_write_file()
            }
        }
    }

    #[cfg_attr(any(target_os = "android", target_env = "ohos"), expect(dead_code))]
    pub(crate) fn experimental_preferences_enabled(&self) -> bool {
        self.experimental_preferences_enabled.get()
    }

    /// Keep the `Sec-CH-Prefers-Color-Scheme` client hint in step with the theme pages are shown
    /// in, so sites that pick light or dark on the server (Google) match.
    pub(crate) fn sync_color_scheme_hint(&self, theme: servo::Theme) {
        let value = match theme {
            servo::Theme::Dark => "dark",
            servo::Theme::Light => "light",
        };
        if self.color_scheme_hint.replace(Some(value)) != Some(value) {
            self.servo
                .set_preference("network_prefers_color_scheme_hint", PrefValue::Str(value.to_owned()));
        }
    }

    #[cfg_attr(target_env = "ohos", expect(dead_code))]
    pub(crate) fn set_experimental_preferences_enabled(&self, new_value: bool) {
        let old_value = self.experimental_preferences_enabled.replace(new_value);
        if old_value == new_value {
            return;
        }
        for pref in EXPERIMENTAL_PREFS {
            self.servo.set_preference(pref, PrefValue::Bool(new_value));
        }
    }

    /// If the user has closed every window, the app is about to exit:
    /// arm the shutdown watchdog now, before anything that could hang (see `shutdown`).
    pub(crate) fn note_close_if_last_window(&self) {
        if self.servoshell_preferences.webdriver_port.get().is_none() &&
            self.windows.borrow().iter().all(|window| window.close_scheduled())
        {
            note_shutdown("last window closed");
        }
    }

    /// Close any [`ServoShellWindow`] that doesn't have an open [`WebView`].
    fn close_empty_windows(&self) {
        let mut move_focus = false;
        self.windows.borrow_mut().retain(|window| {
            if !self.exit_scheduled.get() && !window.should_close() {
                return true;
            }

            if let Some(focused_window) = self.focused_window() &&
                Rc::ptr_eq(window, &focused_window)
            {
                *self.focused_window.borrow_mut() = None;
                move_focus = !focused_window.platform_window().platform_manages_focus();
            }
            false
        });

        // If the platform doesn't manage focus, then focus the most recently opened
        // window.
        if move_focus &&
            let Some(newly_focused_window) = self
                .windows()
                .iter()
                .rev()
                .find(|window| !window.platform_window().platform_manages_focus())
                .cloned()
        {
            self.set_window_has_focus(newly_focused_window, true);
        }
    }

    /// Spins the internal application event loop.
    ///
    /// - Notifies Servo about incoming gamepad events
    /// - Spin the Servo event loop, which will update Servo's embedding layer and trigger
    ///   delegate methods.
    ///
    /// Returns true if the event loop should continue spinning and false if it should exit.
    pub(crate) fn spin_event_loop(
        self: &Rc<Self>,
        create_platform_window: Option<&dyn Fn(Url) -> Rc<dyn PlatformWindow>>,
    ) -> bool {
        self.handle_webdriver_messages(create_platform_window);

        self.note_close_if_last_window();
        self.servo.spin_event_loop();

        // Download progress: refresh the sidebar's downloads indicator and panel.
        #[cfg(not(any(target_os = "android", target_env = "ohos")))]
        if self.downloads.borrow_mut().take_changed() {
            for window in self.windows.borrow().iter() {
                window.set_needs_update();
            }
        }

        for window in self.windows.borrow().iter() {
            window.update_and_request_repaint_if_necessary(self);
        }

        // We clone here to avoid a double borrow. User interface commands can update the list of windows.
        let windows: Vec<_> = self.windows.borrow().iter().cloned().collect();
        for window in windows {
            window.handle_interface_commands(self, create_platform_window);
        }

        let due = self
            .deferred_screenshot
            .borrow()
            .as_ref()
            .is_some_and(|(at, _)| std::time::Instant::now() >= *at);
        if due {
            if let Some((_, webview)) = self.deferred_screenshot.borrow_mut().take() {
                self.take_output_screenshot(webview);
            }
        }

        if self.servoshell_preferences.exit_after_stable_image && self.achieved_stable_image.get() {
            self.schedule_exit();
        }

        self.close_empty_windows();

        // When no more windows are open, exit the application. Do not do this when
        // running WebDriver, which expects to keep running with no WebView open.
        if self.servoshell_preferences.webdriver_port.get().is_none() &&
            self.windows.borrow().is_empty()
        {
            self.schedule_exit()
        }

        !self.exit_scheduled.get()
    }

    fn maybe_window_for_webview(&self, webview: &WebView) -> Option<Rc<ServoShellWindow>> {
        // Look up the ServoShellWindow by RenderingContext. This method can be called while a
        // WebView is being constructed, which means that it may not fully be associated with a
        // ServoShellWindow yet.
        let rendering_context = webview.rendering_context();
        self.windows()
            .iter()
            .find(|window| {
                Rc::ptr_eq(
                    &window.platform_window().rendering_context(),
                    &rendering_context,
                )
            })
            .cloned()
    }

    pub(crate) fn window_for_webview(&self, webview: &WebView) -> Rc<ServoShellWindow> {
        self.maybe_window_for_webview(webview)
            .unwrap_or_else(|| panic!("Looking for unexpected WebView: {:?}", webview.id()))
    }

    pub(crate) fn platform_window_for_webview(&self, webview: &WebView) -> Rc<dyn PlatformWindow> {
        self.window_for_webview(webview).platform_window()
    }

    /// If we are exiting after achieving a stable image or we want to save the display of the
    /// [`WebView`] to an image file, request a screenshot of the [`WebView`].
    fn maybe_request_screenshot(&self, webview: WebView) {
        let output_path = self.servoshell_preferences.output_image_path.clone();
        if !self.servoshell_preferences.exit_after_stable_image && output_path.is_none() {
            return;
        }

        // Never request more than a single screenshot for now.
        if self.achieved_stable_image.get() || self.deferred_screenshot.borrow().is_some() {
            return;
        }

        // Pages that fade their content in after loading look blank at load time, so tools
        // like sitecheck can ask for the screenshot a while later.
        let delay = std::env::var("STILLWATER_SCREENSHOT_DELAY_MS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(0);
        if delay > 0 {
            let at = std::time::Instant::now() + std::time::Duration::from_millis(delay);
            *self.deferred_screenshot.borrow_mut() = Some((at, webview));
            return;
        }
        self.take_output_screenshot(webview);
    }

    fn take_output_screenshot(&self, webview: WebView) {
        let output_path = self.servoshell_preferences.output_image_path.clone();
        let achieved_stable_image = self.achieved_stable_image.clone();

        webview.take_screenshot(None, move |image| {
            achieved_stable_image.set(true);

            let Some(output_path) = output_path else {
                return;
            };

            let image = match image {
                Ok(image) => image,
                Err(error) => {
                    error!("Could not take screenshot: {error:?}");
                    return;
                },
            };

            let image_format = ImageFormat::from_path(&output_path).unwrap_or(ImageFormat::Png);
            if let Err(error) =
                DynamicImage::ImageRgba8(image).save_with_format(output_path, image_format)
            {
                error!("Failed to save screenshot: {error}.");
            }
        });
    }

    pub(crate) fn set_pending_traversal(
        &self,
        traversal_id: TraversalId,
        sender: GenericSender<WebDriverLoadStatus>,
    ) {
        self.webdriver_senders
            .borrow_mut()
            .pending_traversals
            .insert(traversal_id, sender);
    }

    pub(crate) fn set_load_status_sender(
        &self,
        webview_id: WebViewId,
        sender: GenericSender<WebDriverLoadStatus>,
    ) {
        self.webdriver_senders
            .borrow_mut()
            .load_status_senders
            .insert(webview_id, sender);
    }

    fn remove_load_status_sender(&self, webview_id: WebViewId) {
        self.webdriver_senders
            .borrow_mut()
            .load_status_senders
            .remove(&webview_id);
    }

    fn set_script_command_interrupt_sender(
        &self,
        sender: Option<GenericSender<WebDriverJSResult>>,
    ) {
        self.webdriver_senders
            .borrow_mut()
            .script_evaluation_interrupt_sender = sender;
    }

    pub(crate) fn handle_webdriver_input_event(
        &self,
        webview_id: WebViewId,
        input_event: InputEvent,
        response_sender: Option<Sender<()>>,
    ) {
        if let Some(webview) = self.webview_by_id(webview_id) {
            let event_id = webview.notify_input_event(input_event);
            if let Some(response_sender) = response_sender {
                self.pending_webdriver_events
                    .borrow_mut()
                    .insert(event_id, (webview_id, response_sender));
            }
        } else {
            error!("Could not find WebView ({webview_id:?}) for WebDriver event: {input_event:?}");
        };
    }

    pub(crate) fn handle_webdriver_screenshot(
        &self,
        webview_id: WebViewId,
        rect: Option<Rect<f32, CSSPixel>>,
        result_sender: Sender<Result<RgbaImage, ScreenshotCaptureError>>,
    ) {
        if let Some(webview) = self.webview_by_id(webview_id) {
            let rect = rect.map(|rect| rect.to_box2d().into());
            webview.take_screenshot(rect, move |result| {
                if let Err(error) = result_sender.send(result) {
                    warn!("Failed to send response to TakeScreenshot: {error}");
                }
            });
        } else if let Err(error) =
            result_sender.send(Err(ScreenshotCaptureError::WebViewDoesNotExist))
        {
            error!("Failed to send response to TakeScreenshot: {error}");
        }
    }

    pub(crate) fn handle_webdriver_script_command(&self, script_command: &WebDriverScriptCommand) {
        match script_command {
            WebDriverScriptCommand::ExecuteScriptWithCallback(_webview_id, response_sender) => {
                // Give embedder a chance to interrupt the script command.
                // Webdriver only handles 1 script command at a time, so we can
                // safely set a new interrupt sender and remove the previous one here.
                self.set_script_command_interrupt_sender(Some(response_sender.clone()));
            },
            WebDriverScriptCommand::AddLoadStatusSender(webview_id, load_status_sender) => {
                self.set_load_status_sender(*webview_id, load_status_sender.clone());
            },
            WebDriverScriptCommand::RemoveLoadStatusSender(webview_id) => {
                self.remove_load_status_sender(*webview_id);
            },
            _ => {
                self.set_script_command_interrupt_sender(None);
            },
        }
    }

    pub(crate) fn handle_webdriver_load_url(
        &self,
        webview_id: WebViewId,
        url: Url,
        load_status_sender: GenericSender<WebDriverLoadStatus>,
    ) {
        let Some(webview) = self.webview_by_id(webview_id) else {
            return;
        };

        self.platform_window_for_webview(&webview)
            .dismiss_embedder_controls_for_webview(webview_id);

        info!("Loading URL in webview {}: {}", webview_id, url);
        self.set_load_status_sender(webview_id, load_status_sender);
        webview.load(url);
    }

    #[cfg(all(
        feature = "gamepad",
        not(any(target_os = "android", target_env = "ohos"))
    ))]
    pub(crate) fn handle_gamepad_events(
        &self,
        event: Event,
        gamepad_name: String,
        gamepad_index: GamepadIndex,
    ) {
        let Some(gamepad_delegate) = self.gamepad_delegate.as_ref() else {
            return;
        };
        let Some(active_webview) = self
            .focused_window()
            .and_then(|window| window.active_webview())
        else {
            return;
        };
        gamepad_delegate.handle_gamepad_events(event, gamepad_name, gamepad_index, active_webview);
    }

    /// Interrupt any ongoing WebDriver-based script evaluation.
    ///
    /// From <https://w3c.github.io/webdriver/#dfn-execute-a-function-body>:
    /// > The rules to execute a function body are as follows. The algorithm returns
    /// > an ECMAScript completion record.
    /// >
    /// > If at any point during the algorithm a user prompt appears, immediately return
    /// > Completion { Type: normal, Value: null, Target: empty }, but continue to run the
    /// >  other steps of this algorithm in parallel.
    fn interrupt_webdriver_script_evaluation(&self) {
        if let Some(sender) = &self
            .webdriver_senders
            .borrow()
            .script_evaluation_interrupt_sender
        {
            sender.send(Ok(JSValue::Null)).unwrap_or_else(|err| {
                info!(
                    "Notify dialog appear failed. Maybe the channel to webdriver is closed: {err}"
                );
            });
        }
    }

    #[cfg(not(any(target_os = "android", target_env = "ohos")))]
    pub(crate) fn set_accessibility_active(&self, active: bool) {
        let was_active = self.accessibility_active.replace(active);
        if active == was_active {
            return;
        }

        for window in self.windows().iter() {
            for (_, webview) in window.webviews() {
                // Activate accessibility in the WebView.
                // There are two sites like this; this is the a11y activation site.
                webview.set_accessibility_active(active);
            }
        }
    }

    /// A tab's recent script errors and warnings (oldest first), for broken-site reports.
    pub(crate) fn recent_console_errors(&self, webview: WebViewId) -> Vec<String> {
        self.recent_console_errors
            .borrow()
            .get(&webview)
            .map(|list| list.iter().cloned().collect())
            .unwrap_or_default()
    }

    pub(crate) fn accessibility_active(&self) -> bool {
        self.accessibility_active.get()
    }

    /// Tabs that are currently playing audio or video (reported progress in the last 10 s).
    pub(crate) fn playing_media(&self) -> HashSet<WebViewId> {
        let mut playing = self.playing_media.borrow_mut();
        playing.retain(|_, last| last.elapsed() < std::time::Duration::from_secs(10));
        playing.keys().copied().collect()
    }

    /// Collect garbage once activity settles: pages allocate a lot while loading, and without
    /// this the memory is only reclaimed when some page next allocates enough to trigger a GC.
    fn schedule_idle_gc(&self) {
        self.gc_due
            .set(Some(std::time::Instant::now() + IDLE_GC_DELAY));
    }

    /// When the next idle garbage collection is due, if one is scheduled.
    pub(crate) fn idle_gc_due(&self) -> Option<std::time::Instant> {
        self.gc_due.get()
    }

    /// Run the scheduled idle garbage collection if it's due.
    pub(crate) fn maybe_collect_garbage(&self) {
        if self
            .gc_due
            .get()
            .is_some_and(|due| std::time::Instant::now() >= due)
        {
            // Not while a page loads (the load's own completion schedules it again). A load
            // that never finishes stops holding it back after a while.
            let loading = {
                let mut loads = self.loads_in_progress.borrow_mut();
                loads.retain(|_, started| started.elapsed() < MAX_LOAD_HOLDING_GC);
                !loads.is_empty()
            };
            if loading {
                self.schedule_idle_gc();
                return;
            }
            self.gc_due.set(None);
            #[cfg(not(any(target_os = "android", target_env = "ohos")))]
            crate::desktop::diagnostics::log("idle garbage collection");
            // When a page was replaced or a tab closed, also drop the renderer's caches: its
            // textures for the old pages are only evicted as new frames are drawn, so an idle
            // window otherwise keeps hundreds of megabytes of them. The next frame redraws its
            // glyphs and images, once.
            if self.render_caches_stale.replace(false) {
                self.servo.notify_memory_pressure();
            } else {
                self.servo.collect_garbage();
            }
        }
    }
}

/// How long after the last page load (or tab close) to collect garbage.
const IDLE_GC_DELAY: std::time::Duration = std::time::Duration::from_secs(8);
/// A page load older than this no longer postpones the idle garbage collection.
const MAX_LOAD_HOLDING_GC: std::time::Duration = std::time::Duration::from_secs(30);

impl WebViewDelegate for RunningAppState {
    fn load_web_resource(&self, webview: WebView, load: servo::WebResourceLoad) {
        // Only main pages reach here (see adblock::filter_request): get the site's adblock
        // scriptlets ready before its document is created. Dropping load lets it proceed.
        #[cfg(not(any(target_os = "android", target_env = "ohos")))]
        if load.request.is_for_main_frame {
            // The tab shows as loading from now until the page completes.
            crate::desktop::sidebar::navigation_started(webview.id());
            self.adblock
                .borrow_mut()
                .on_main_frame_request(webview.id(), load.request.url.as_url());
            self.window_for_webview(&webview).set_needs_update();
        }
        let _ = (&webview, &load);
    }

    fn request_navigation(&self, webview: WebView, request: servo::NavigationRequest) {
        // Alt+click on a link opens it in Glance instead of navigating the tab.
        #[cfg(not(any(target_os = "android", target_env = "ohos")))]
        if crate::desktop::panes::divert_to_glance(&request.url) {
            request.deny();
            self.window_for_webview(&webview).set_needs_update();
            return;
        }
        // Web pages can't send a tab (or a popup) to the browser's own pages: only the user
        // and those pages themselves can.
        if request.url.scheme() == "servo" &&
            webview.url().is_some_and(|url| !matches!(url.scheme(), "servo" | "resource"))
        {
            request.deny();
            return;
        }
        // A link or script navigation: show the tab as loading right away.
        #[cfg(not(any(target_os = "android", target_env = "ohos")))]
        {
            if !crate::desktop::sidebar::is_same_document(webview.url(), &request.url) {
                crate::desktop::sidebar::navigation_started(webview.id());
                self.window_for_webview(&webview).set_needs_update();
            }
        }
        request.allow();
    }
    fn screen_geometry(&self, webview: WebView) -> Option<servo::ScreenGeometry> {
        Some(self.platform_window_for_webview(&webview).screen_geometry())
    }

    fn notify_status_text_changed(&self, webview: WebView, _status: Option<String>) {
        self.window_for_webview(&webview).set_needs_update();
    }

    fn notify_history_changed(&self, webview: WebView, _entries: Vec<Url>, _current: usize) {
        self.window_for_webview(&webview).set_needs_update();
    }

    fn notify_url_changed(&self, webview: WebView, url: Url) {
        // Record the visit in history (tabs only: split view and Glance have their own delegate).
        #[cfg(not(any(target_os = "android", target_env = "ohos")))]
        self.history.borrow_mut().on_url_changed(webview.id(), &url);
        let _ = (&webview, &url);
    }

    fn notify_page_title_changed(&self, webview: WebView, _: Option<String>) {
        #[cfg(not(any(target_os = "android", target_env = "ohos")))]
        self.history.borrow_mut().on_title(&webview);
        self.window_for_webview(&webview).set_needs_update();
    }

    fn notify_find_in_page_result(&self, webview: WebView, result: servo::FindInPageResult) {
        #[cfg(not(any(target_os = "android", target_env = "ohos")))]
        crate::desktop::find_bar::record_result(webview.id(), result);
        let _ = result;
        self.window_for_webview(&webview).set_needs_update();
    }

    fn notify_traversal_complete(&self, _webview: WebView, traversal_id: TraversalId) {
        let mut webdriver_state = self.webdriver_senders.borrow_mut();
        if let Entry::Occupied(entry) = webdriver_state.pending_traversals.entry(traversal_id) {
            let sender = entry.remove();
            let _ = sender.send(WebDriverLoadStatus::Complete);
        }
    }

    fn request_move_to(&self, webview: WebView, new_position: DeviceIntPoint) {
        self.platform_window_for_webview(&webview)
            .set_position(new_position);
    }

    fn request_resize_to(&self, webview: WebView, requested_outer_size: DeviceIntSize) {
        self.platform_window_for_webview(&webview)
            .request_resize(&webview, requested_outer_size);
    }

    fn request_authentication(
        &self,
        webview: WebView,
        authentication_request: AuthenticationRequest,
    ) {
        self.platform_window_for_webview(&webview)
            .show_http_authentication_dialog(webview.id(), authentication_request);
    }

    fn request_download(&self, webview: WebView, request: servo::DownloadRequest) {
        #[cfg(not(any(target_os = "android", target_env = "ohos")))]
        {
            // Headless and WebDriver runs are automated tests and site sweeps: only save files
            // when they choose a download folder, never into the user's.
            let automated = self.servoshell_preferences.headless ||
                self.servoshell_preferences.webdriver_port.get().is_some();
            if automated && std::env::var_os("STILLWATER_DOWNLOAD_DIR").is_none() {
                crate::desktop::diagnostics::log(format!(
                    "download not saved (automated run without STILLWATER_DOWNLOAD_DIR): {}",
                    request.suggested_filename()
                ));
                request.cancel();
                return;
            }
            // A link that turned out to be a file: the tab's page doesn't change.
            crate::desktop::sidebar::navigation_finished(webview.id());
            self.downloads.borrow_mut().start(request);
            self.window_for_webview(&webview).set_needs_update();
        }
        #[cfg(any(target_os = "android", target_env = "ohos"))]
        let _ = (webview, request);
    }

    fn request_create_new(&self, parent_webview: WebView, request: CreateNewWebViewRequest) {
        let window = self.window_for_webview(&parent_webview);

        // When WebDriver wants to open a new WebView, do this in a new window. Normally, servoshell
        // throttles WebViews that are hidden, but WebDriver expects all opened WebViews to stay
        // active. Using a separate window ensures this.
        if self.servoshell_preferences.webdriver_port.get().is_some() {
            window.queue_user_interface_command(UserInterfaceCommand::NewWindow(
                TopLevelWebViewCreationRequest::WithCreateRequest(request),
            ));
            return;
        }

        let platform_window = window.platform_window();
        let webview = request
            .builder(platform_window.rendering_context())
            .hidpi_scale_factor(platform_window.hidpi_scale_factor())
            .delegate(parent_webview.delegate())
            .build();

        window.add_webview(webview.clone());
        window.activate_webview(webview.id());
    }

    fn notify_media_session_event(&self, webview: WebView, event: servo::MediaSessionEvent) {
        // Pages often have several media elements (YouTube pauses its spare players while the
        // main one plays), so a "paused" event doesn't mean the tab went quiet. Instead, a tab
        // counts as playing while it keeps reporting progress (about twice a second).
        let playing = match event {
            servo::MediaSessionEvent::PlaybackStateChange(state) => {
                matches!(state, servo::MediaSessionPlaybackState::Playing)
            },
            servo::MediaSessionEvent::SetPositionState(state) => state.playback_rate != 0.0,
            servo::MediaSessionEvent::SetMetadata(_) => false,
        };
        if playing &&
            self.playing_media
                .borrow_mut()
                .insert(webview.id(), std::time::Instant::now())
                .is_none()
        {
            // Show the tab's speaker icon.
            self.window_for_webview(&webview).set_needs_update();
        }
    }

    fn notify_closed(&self, webview: WebView) {
        let webview_id = webview.id();
        #[cfg(not(any(target_os = "android", target_env = "ohos")))]
        crate::desktop::sidebar::navigation_finished(webview_id);
        self.playing_media.borrow_mut().remove(&webview_id);
        self.loads_in_progress.borrow_mut().remove(&webview_id);
        self.render_caches_stale.set(true);
        self.schedule_idle_gc();
        self.pending_webdriver_events
            .borrow_mut()
            .retain(|_, (pending_webview_id, _)| *pending_webview_id != webview_id);
        self.window_for_webview(&webview).close_webview(webview_id)
    }

    fn notify_input_event_handled(
        &self,
        webview: WebView,
        id: InputEventId,
        result: InputEventResult,
    ) {
        self.platform_window_for_webview(&webview)
            .notify_input_event_handled(&webview, id, result);

        self.pending_webdriver_events.borrow_mut().remove(&id);
    }

    fn notify_cursor_changed(&self, webview: WebView, cursor: servo::Cursor) {
        self.platform_window_for_webview(&webview)
            .set_cursor(cursor);
    }

    fn notify_load_status_changed(&self, webview: WebView, status: LoadStatus) {
        #[cfg(not(any(target_os = "android", target_env = "ohos")))]
        crate::desktop::diagnostics::log(format!(
            "page load {status:?}: {}",
            webview.url().map(|url| url.to_string()).unwrap_or_default()
        ));
        self.window_for_webview(&webview).set_needs_update();
        // A new page has no media playing yet.
        if status == LoadStatus::Started {
            self.playing_media.borrow_mut().remove(&webview.id());
            // The saved-login list belongs to the page that's going away.
            #[cfg(not(any(target_os = "android", target_env = "ohos")))]
            self.passwords.borrow_mut().close_chooser(webview.id());
            self.render_caches_stale.set(true);
            self.loads_in_progress
                .borrow_mut()
                .insert(webview.id(), std::time::Instant::now());
            // An idle collection from an earlier load would land in the middle of this one.
            if self.gc_due.get().is_some() {
                self.schedule_idle_gc();
            }
        }
        if matches!(status, LoadStatus::Complete | LoadStatus::Stopped) {
            self.loads_in_progress.borrow_mut().remove(&webview.id());
        }
        #[cfg(not(any(target_os = "android", target_env = "ohos")))]
        self.translation.borrow_mut().on_load(&webview, status);
        #[cfg(not(any(target_os = "android", target_env = "ohos")))]
        if matches!(status, LoadStatus::Complete | LoadStatus::Stopped) {
            crate::desktop::sidebar::navigation_finished(webview.id());
        }
        if status == LoadStatus::Complete {
            self.schedule_idle_gc();
            #[cfg(not(any(target_os = "android", target_env = "ohos")))]
            self.history.borrow_mut().on_title(&webview);
        }

        let webdriver_status = match status {
            LoadStatus::Complete => WebDriverLoadStatus::Complete,
            LoadStatus::Stopped => WebDriverLoadStatus::NavigationStop,
            _ => return,
        };

        if let Some(sender) = self
            .webdriver_senders
            .borrow_mut()
            .load_status_senders
            .remove(&webview.id())
        {
            let _ = sender.send(webdriver_status);
        }
        self.maybe_request_screenshot(webview);
    }

    fn notify_fullscreen_state_changed(&self, webview: WebView, fullscreen_state: bool) {
        self.platform_window_for_webview(&webview)
            .set_fullscreen(fullscreen_state);
    }

    fn show_bluetooth_device_dialog(
        &self,
        webview: WebView,
        request: BluetoothDeviceSelectionRequest,
    ) {
        self.platform_window_for_webview(&webview)
            .show_bluetooth_device_dialog(webview.id(), request);
    }

    fn request_permission(&self, webview: WebView, permission_request: PermissionRequest) {
        self.platform_window_for_webview(&webview)
            .show_permission_dialog(webview.id(), permission_request);
    }

    fn notify_new_frame_ready(&self, webview: WebView) {
        self.window_for_webview(&webview).set_needs_repaint();
    }

    fn show_embedder_control(&self, webview: WebView, embedder_control: EmbedderControl) {
        if self.servoshell_preferences.webdriver_port.get().is_some() {
            if matches!(&embedder_control, EmbedderControl::SimpleDialog(..)) {
                self.interrupt_webdriver_script_evaluation();

                // Dialogs block the page load, so need need to notify WebDriver
                if let Some(sender) = self
                    .webdriver_senders
                    .borrow_mut()
                    .load_status_senders
                    .get(&webview.id())
                {
                    let _ = sender.send(WebDriverLoadStatus::Blocked);
                };
            }

            self.webdriver_embedder_controls
                .show_embedder_control(webview.id(), embedder_control);
            return;
        }

        self.window_for_webview(&webview)
            .show_embedder_control(webview, embedder_control);
    }

    fn hide_embedder_control(&self, webview: WebView, embedder_control_id: EmbedderControlId) {
        if self.servoshell_preferences.webdriver_port.get().is_some() {
            self.webdriver_embedder_controls
                .hide_embedder_control(webview.id(), embedder_control_id);
            return;
        }

        self.window_for_webview(&webview)
            .hide_embedder_control(webview, embedder_control_id);
    }

    fn notify_favicon_changed(&self, webview: WebView) {
        self.window_for_webview(&webview)
            .notify_favicon_changed(webview);
    }

    fn notify_crashed(&self, webview: WebView, reason: String, backtrace: Option<String>) {
        self.platform_window_for_webview(&webview)
            .notify_crashed(webview, reason, backtrace);
    }

    fn show_console_message(&self, webview: WebView, level: ConsoleLogLevel, message: String) {
        // Class/id reports from the adblock collector aren't real console output.
        #[cfg(not(any(target_os = "android", target_env = "ohos")))]
        if let Some(payload) =
            message.strip_prefix(crate::desktop::adblock::COSMETIC_MESSAGE_PREFIX)
        {
            self.adblock.borrow().on_cosmetic_message(&webview, payload);
            return;
        }
        // The settings page (only from a tab that really shows servo:settings: a web page can
        // print the same prefix).
        #[cfg(not(any(target_os = "android", target_env = "ohos")))]
        if let Some(payload) = message.strip_prefix(crate::desktop::sidebar::SETTINGS_MESSAGE_PREFIX) {
            if webview.url().is_some_and(|url| crate::desktop::sidebar::is_settings_url(&url)) &&
                let Some(headed_window) =
                    self.window_for_webview(&webview).platform_window().as_headed_window()
            {
                headed_window.with_sidebar(|sidebar| sidebar.post_from_settings(&webview, payload));
            }
            return;
        }
        // Sign-in forms and submitted logins: these carry passwords, so they're never printed
        // or logged.
        #[cfg(not(any(target_os = "android", target_env = "ohos")))]
        if let Some(payload) = message.strip_prefix(crate::desktop::passwords::MESSAGE_PREFIX) {
            if self.passwords.borrow_mut().on_page_message(&webview, payload) {
                self.window_for_webview(&webview).set_needs_update();
            }
            return;
        }
        if matches!(level, ConsoleLogLevel::Error | ConsoleLogLevel::Warn) {
            let mut errors = self.recent_console_errors.borrow_mut();
            let list = errors.entry(webview.id()).or_default();
            if list.len() >= 20 {
                list.pop_front();
            }
            list.push_back(message.clone());
        }
        self.platform_window_for_webview(&webview)
            .show_console_message(level, &message);
    }

    fn notify_accessibility_tree_update(
        &self,
        webview: WebView,
        tree_update: accesskit::TreeUpdate,
    ) {
        self.platform_window_for_webview(&webview)
            .notify_accessibility_tree_update(webview, tree_update);
    }
}

struct ServoShellServoDelegate {
    #[cfg(not(any(target_os = "android", target_env = "ohos")))]
    downloads: crate::desktop::downloads::SharedDownloads,
}

impl ServoDelegate for ServoShellServoDelegate {
    fn notify_download_update(&self, id: servo::DownloadId, update: servo::DownloadUpdate) {
        #[cfg(not(any(target_os = "android", target_env = "ohos")))]
        self.downloads.borrow_mut().update(id, update);
        #[cfg(any(target_os = "android", target_env = "ohos"))]
        let _ = (id, update);
    }

    fn notify_devtools_server_started(&self, port: u16, _token: String) {
        info!("Devtools Server running on port {port}");
    }

    fn request_devtools_connection(&self, request: AllowOrDenyRequest) {
        request.allow();
    }

    fn notify_error(&self, error: ServoError) {
        error!("Saw Servo error: {error:?}!");
    }

    fn show_console_message(&self, level: ConsoleLogLevel, message: String) {
        // For messages without a WebView context, apply platform-specific behavior
        #[cfg(not(any(target_os = "android", target_env = "ohos")))]
        println!("{message}");
        log::log!(level.into(), "{message}");
    }
}

/// Arm the desktop shutdown watchdog (`desktop::shutdown`).
fn note_shutdown(reason: &str) {
    #[cfg(not(any(target_os = "android", target_env = "ohos")))]
    crate::desktop::shutdown::begin(reason);
    #[cfg(any(target_os = "android", target_env = "ohos"))]
    let _ = reason;
}
