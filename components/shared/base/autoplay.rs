/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Stillwater: WebViews whose media may not start playing until the user interacts with them.
//!
//! The embedder marks a WebView whose page it loads without the user asking for that page right
//! now (a tab restored from the last session, an unloaded tab loaded again when selected).
//! `HTMLMediaElement::play()` and the `autoplay` attribute are then refused until a real click or
//! key press in one of the WebView's documents (user activation), which lifts the block, as does
//! the embedder loading a new URL into the WebView.
//!
//! This is a process-wide table: it works when script runs in the embedder's process (Servo's
//! default). With `--multiprocess` the content processes never see the block, so media plays as
//! before.

use std::sync::Mutex;

use crate::id::WebViewId;

static BLOCKED: Mutex<Vec<WebViewId>> = Mutex::new(Vec::new());

/// Refuse media playback in `webview` until the user interacts with it.
pub fn block_until_activation(webview: WebViewId) {
    if let Ok(mut blocked) = BLOCKED.lock() &&
        !blocked.contains(&webview)
    {
        blocked.push(webview);
    }
}

/// Allow media playback in `webview` again.
pub fn unblock(webview: WebViewId) {
    if let Ok(mut blocked) = BLOCKED.lock() {
        blocked.retain(|other| *other != webview);
    }
}

/// Whether media in `webview` must wait for the user.
pub fn is_blocked(webview: WebViewId) -> bool {
    BLOCKED
        .lock()
        .map(|blocked| blocked.contains(&webview))
        .unwrap_or(false)
}
