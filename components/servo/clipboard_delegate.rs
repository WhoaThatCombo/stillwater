/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use servo_base::generic_channel::GenericCallback;

use crate::WebView;

pub struct StringRequest {
    pub(crate) result_sender: GenericCallback<Result<String, String>>,
    response_sent: bool,
}

impl StringRequest {
    pub fn success(mut self, string: String) {
        let _ = self.result_sender.send(Ok(string));
        self.response_sent = true;
    }

    pub fn failure(mut self, message: String) {
        let _ = self.result_sender.send(Err(message));
        self.response_sent = true;
    }
}

impl From<GenericCallback<Result<String, String>>> for StringRequest {
    fn from(result_sender: GenericCallback<Result<String, String>>) -> Self {
        Self {
            result_sender,
            response_sent: false,
        }
    }
}

impl Drop for StringRequest {
    fn drop(&mut self) {
        if !self.response_sent {
            let _ = self
                .result_sender
                .send(Err("No response sent to request.".into()));
        }
    }
}

/// A delegate that is responsible for accessing the system clipboard. On Mac, Windows, and
/// Linux if the `clipboard` feature is enabled, a default delegate is automatically used
/// that implements clipboard support. An embedding application can override this delegate
/// by using this trait.
pub trait ClipboardDelegate {
    /// A request to clear all contents of the system clipboard.
    fn clear(&self, _webview: WebView) {}

    /// A request to get the text contents of the system clipboard. Once the contents are
    /// retrieved the embedder should call [`StringRequest::success`] with the text or
    /// [`StringRequest::failure`] with a failure message.
    fn get_text(&self, _webview: WebView, _request: StringRequest) {}

    /// A request to set the text contents of the system clipboard to `new_contents`.
    fn set_text(&self, _webview: WebView, _new_contents: String) {}

    /// A request to set the system clipboard to formatted text: `text` as plain text and
    /// `html`, an HTML fragment, for applications that paste formatting. The default
    /// implementation sets only the plain text.
    fn set_html(&self, webview: WebView, text: String, _html: String) {
        self.set_text(webview, text);
    }
}

pub(crate) struct DefaultClipboardDelegate;

impl ClipboardDelegate for DefaultClipboardDelegate {
    fn clear(&self, _webview: WebView) {
        if test_mode() {
            return fallback_clipboard::clear();
        }
        clipboard::clear();
    }

    fn get_text(&self, _webview: WebView, request: StringRequest) {
        if test_mode() {
            return fallback_clipboard::get_text(request);
        }
        clipboard::get_text(request);
    }

    fn set_text(&self, _webview: WebView, new_contents: String) {
        if test_mode() {
            log::warn!("Stillwater clipboard (test mode) text/plain: {new_contents:?}");
            fallback_clipboard::set_text(new_contents);
            return;
        }
        clipboard::set_text(new_contents);
    }

    fn set_html(&self, _webview: WebView, text: String, html: String) {
        if test_mode() {
            log::warn!("Stillwater clipboard (test mode) text/plain: {text:?}");
            log::warn!("Stillwater clipboard (test mode) text/html: {html:?}");
            fallback_clipboard::set_text(text);
            return;
        }
        clipboard::set_html(text, html);
    }
}

/// Stillwater: with the `dom_clipboard_test_mode` pref, the default delegate never touches
/// the system clipboard. It keeps the contents in memory (so paste in the same process gets
/// them back) and logs what would have been written, so automated tests can check copy
/// without reading or replacing the user's clipboard.
fn test_mode() -> bool {
    servo_config::pref!(dom_clipboard_test_mode)
}

mod fallback_clipboard {
    use std::sync::{LockResult, Mutex, OnceLock};

    use crate::clipboard_delegate::StringRequest;

    /// If the clipboard cannot be accessed, we fall back to a simple `String` to store
    /// text for the clipboard. This obviously does not work across processes.
    static SHARED_FALLBACK_CLIPBOARD: OnceLock<Mutex<String>> = OnceLock::new();

    fn with_shared_clipboard(callback: impl FnOnce(&mut String)) {
        let clipboard_mutex =
            SHARED_FALLBACK_CLIPBOARD.get_or_init(|| Mutex::new(Default::default()));
        if let LockResult::Ok(mut string) = clipboard_mutex.lock() {
            callback(&mut string)
        }
    }

    pub(super) fn clear() {
        with_shared_clipboard(|clipboard_string| {
            clipboard_string.clear();
        });
    }

    pub(super) fn get_text(request: StringRequest) {
        with_shared_clipboard(move |clipboard_string| request.success(clipboard_string.clone()));
    }

    pub(super) fn set_text(new_contents: String) {
        with_shared_clipboard(move |clipboard_string| {
            *clipboard_string = new_contents;
        });
    }
}

#[cfg(all(
    feature = "clipboard",
    not(any(target_os = "android", target_env = "ohos"))
))]
mod clipboard {
    use std::sync::OnceLock;

    use arboard::Clipboard;
    use parking_lot::Mutex;

    use super::StringRequest;
    use crate::clipboard_delegate::fallback_clipboard;

    /// A shared clipboard for use by the [`DefaultClipboardDelegate`](super::DefaultClipboardDelegate).
    /// This is protected by a mutex so that it can only be used by one thread at a time.
    /// The `arboard` documentation suggests that more than one thread shouldn't try to access
    /// the Windows clipboard at a time. See <https://docs.rs/arboard/latest/arboard/struct.Clipboard.html>.
    static SHARED_CLIPBOARD: OnceLock<Option<Mutex<Clipboard>>> = OnceLock::new();

    fn with_shared_clipboard<ResultType>(
        callback: impl FnOnce(&mut Clipboard) -> Result<ResultType, arboard::Error>,
    ) -> Result<ResultType, arboard::Error> {
        match SHARED_CLIPBOARD.get_or_init(|| Clipboard::new().ok().map(Mutex::new)) {
            Some(clipboard_mutex) => callback(&mut clipboard_mutex.lock()),
            None => Err(arboard::Error::ClipboardNotSupported),
        }
    }

    pub(super) fn clear() {
        if with_shared_clipboard(|clipboard| clipboard.clear()).is_err() {
            fallback_clipboard::clear();
        }
    }

    pub(super) fn get_text(request: StringRequest) {
        if let Ok(text) = with_shared_clipboard(|clipboard| clipboard.get_text()) {
            request.success(text);
            return;
        };
        fallback_clipboard::get_text(request);
    }

    pub(super) fn set_text(new_contents: String) {
        if with_shared_clipboard(|clipboard| clipboard.set_text(&new_contents)).is_err() {
            fallback_clipboard::set_text(new_contents);
        }
    }

    pub(super) fn set_html(text: String, html: String) {
        if with_shared_clipboard(|clipboard| clipboard.set_html(&html, Some(&text))).is_err() {
            set_text(text);
        }
    }
}

#[cfg(all(feature = "clipboard", target_env = "ohos"))]
mod clipboard {
    use super::StringRequest;
    use crate::clipboard_delegate::fallback_clipboard;

    pub(super) fn clear() {
        if let Err(error) = ohos_pasteboard::clear() {
            log::warn!(
                "OHOS pasteboard clear failed ({error}); using in-memory fallback_clipboard"
            );
            fallback_clipboard::clear();
        }
    }

    pub(super) fn get_text(request: StringRequest) {
        match ohos_pasteboard::get_text() {
            Ok(text) => request.success(text),
            Err(ohos_pasteboard::Error::NoText) => request.success(String::new()),
            Err(error) => {
                log::warn!(
                    "OHOS pasteboard get_text failed ({error}); using in-memory fallback_clipboard"
                );
                fallback_clipboard::get_text(request);
            },
        }
    }

    pub(super) fn set_text(new_contents: String) {
        if let Err(error) = ohos_pasteboard::set_text(&new_contents) {
            log::warn!(
                "OHOS pasteboard set_text failed ({error}); using in-memory fallback_clipboard"
            );
            fallback_clipboard::set_text(new_contents);
        }
    }

    pub(super) fn set_html(text: String, _html: String) {
        set_text(text);
    }
}

#[cfg(any(
    not(feature = "clipboard"),
    all(feature = "clipboard", target_os = "android")
))]
mod clipboard {
    use super::StringRequest;
    use crate::clipboard_delegate::fallback_clipboard;

    pub(super) fn clear() {
        fallback_clipboard::clear();
    }

    pub(super) fn get_text(request: StringRequest) {
        fallback_clipboard::get_text(request);
    }

    pub(super) fn set_text(new_contents: String) {
        fallback_clipboard::set_text(new_contents);
    }

    pub(super) fn set_html(text: String, _html: String) {
        fallback_clipboard::set_text(text);
    }
}
