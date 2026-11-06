/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use crate::dom::bindings::str::DOMString;

#[expect(non_snake_case)]
pub(crate) fn Product() -> DOMString {
    DOMString::from_static("Gecko")
}

#[expect(non_snake_case)]
pub(crate) fn ProductSub() -> DOMString {
    DOMString::from_static("20100101")
}

#[expect(non_snake_case)]
pub(crate) fn Vendor() -> DOMString {
    DOMString::new()
}

#[expect(non_snake_case)]
pub(crate) fn VendorSub() -> DOMString {
    DOMString::new()
}

#[expect(non_snake_case)]
pub(crate) fn TaintEnabled() -> bool {
    false
}

#[expect(non_snake_case)]
pub(crate) fn AppName() -> DOMString {
    DOMString::from_static("Netscape") // Like Gecko/Webkit
}

#[expect(non_snake_case)]
pub(crate) fn AppCodeName() -> DOMString {
    DOMString::from_static("Mozilla")
}

#[expect(non_snake_case)]
#[cfg(target_os = "windows")]
pub(crate) fn Platform() -> DOMString {
    DOMString::from_static("Win32")
}

#[expect(non_snake_case)]
#[cfg(any(target_os = "android", target_os = "linux", target_os = "freebsd"))]
pub(crate) fn Platform() -> DOMString {
    DOMString::from_static("Linux")
}

#[expect(non_snake_case)]
#[cfg(target_os = "macos")]
pub(crate) fn Platform() -> DOMString {
    DOMString::from_static("Mac")
}

#[expect(non_snake_case)]
#[cfg(target_os = "ios")]
pub(crate) fn Platform() -> DOMString {
    DOMString::from_static("iOS")
}

#[expect(non_snake_case)]
pub(crate) fn UserAgent(user_agent: &str) -> DOMString {
    DOMString::from(user_agent)
}

/// <https://html.spec.whatwg.org/multipage/#dom-navigator-appversion>
///
/// "4.0" is allowed, but libraries still in wide use treat it as Netscape 4: jsbn's random
/// number generator then calls `crypto.random()`, which no browser has anymore, and throws
/// (as on web.telegram.org). Report "5.0" like every current engine, with Gecko's platform
/// token to match the user agent string.
#[expect(non_snake_case)]
pub(crate) fn AppVersion() -> DOMString {
    if cfg!(target_os = "windows") {
        DOMString::from_static("5.0 (Windows)")
    } else if cfg!(target_os = "macos") {
        DOMString::from_static("5.0 (Macintosh)")
    } else if cfg!(target_os = "android") {
        DOMString::from_static("5.0 (Android 10)")
    } else {
        DOMString::from_static("5.0 (X11)")
    }
}

#[expect(non_snake_case)]
pub(crate) fn Language() -> DOMString {
    DOMString::from(net_traits::get_current_locale().0.clone())
}
