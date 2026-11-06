/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! "Report a broken site": opens a pre-filled GitHub issue describing the current page.
//! Nothing is sent automatically; the user reviews the issue before submitting it.

use std::process::Command;

use url::Url;

use crate::desktop::updater::CURRENT_VERSION;

const NEW_ISSUE: &str = "https://github.com/WhoaThatCombo/stillwater/issues/new";

fn encode(text: &str) -> String {
    url::form_urlencoded::byte_serialize(text.as_bytes()).collect()
}

/// The GitHub "new issue" URL for a broken-site report. The query string and fragment are
/// dropped from the site's address, since they can contain personal data.
pub(crate) fn issue_url(site: &Url, errors: &[String]) -> String {
    let mut clean = site.clone();
    clean.set_query(None);
    clean.set_fragment(None);
    let host = clean.host_str().unwrap_or("site");

    let mut body = format!(
        "**Site:** {clean}\n**Stillwater:** {CURRENT_VERSION} (Windows)\n\n\
         **What's wrong?**\n<!-- e.g. blank page, missing images, a button does nothing -->\n\n"
    );
    if !errors.is_empty() {
        body.push_str("<details><summary>Recent script errors on this page</summary>\n\n```\n");
        for error in errors.iter().take(15) {
            let line: String = error.chars().take(300).collect();
            body.push_str(&line);
            body.push('\n');
        }
        body.push_str("```\n</details>\n\n");
    }
    body.push_str(&format!(
        "To reproduce: `python tools/sitecheck/sitecheck.py --site {clean}`\n"
    ));

    format!(
        "{NEW_ISSUE}?labels=site-compat&title={}&body={}",
        encode(&format!("Broken site: {host}")),
        encode(&body)
    )
}

/// Whether Windows is set to open https links with Stillwater.
fn stillwater_is_default_browser() -> bool {
    let output = Command::new("reg")
        .args([
            "query",
            r"HKCU\Software\Microsoft\Windows\Shell\Associations\UrlAssociations\https\UserChoice",
            "/v",
            "ProgId",
        ])
        .output();
    output.is_ok_and(|output| String::from_utf8_lossy(&output.stdout).contains("StillwaterURL"))
}

/// Open a URL in another browser: the default one, or Microsoft Edge when Stillwater is the
/// default (GitHub's issue form doesn't fully work in Servo yet).
pub(crate) fn open_externally(url: &str) {
    let target = if stillwater_is_default_browser() {
        format!("microsoft-edge:{url}")
    } else {
        url.to_owned()
    };
    #[cfg(target_os = "windows")]
    {
        use windows_sys::Win32::UI::Shell::ShellExecuteW;
        use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
        let wide = |text: &str| text.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
        let (verb, file) = (wide("open"), wide(&target));
        // ShellExecute passes the URL to the handler intact (no shell parsing of `&`).
        let result = unsafe {
            ShellExecuteW(
                std::ptr::null_mut(),
                verb.as_ptr(),
                file.as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                SW_SHOWNORMAL,
            )
        };
        if (result as isize) <= 32 {
            log::warn!("Couldn't open {target} (ShellExecute returned {})", result as isize);
        }
    }
    #[cfg(not(target_os = "windows"))]
    log::warn!("Opening {target} externally is only implemented on Windows");
}