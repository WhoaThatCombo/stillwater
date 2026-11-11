/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! "Save layout report for this element" (page context menu): writes how the element under the
//! pointer and its surroundings were laid out to a text file next to stillwater.log, for bug
//! reports about boxes drawn at the wrong size. The report leaves out the page's text, ids,
//! class names and full URLs (see layout-report.js). Nothing is sent anywhere.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use servo::{JSValue, WebView};

const REPORT_JS: &str = include_str!("layout-report.js");

/// Measure the element at `(x, y)` (CSS px in the page) and save the report; the status line
/// says where it went.
pub(crate) fn save(webview: &WebView, (x, y): (f32, f32)) {
    let script = REPORT_JS
        .replace("__X__", &format!("{x:.1}"))
        .replace("__Y__", &format!("{y:.1}"));
    webview.evaluate_javascript(script, move |result| {
        let notice = match result {
            Ok(JSValue::String(report)) => match write(&report) {
                Ok(path) => {
                    crate::desktop::diagnostics::log(format!(
                        "layout report saved: {}",
                        path.display()
                    ));
                    format!("Layout report saved: {}", path.display())
                },
                Err(error) => format!("Couldn't save the layout report: {error}"),
            },
            Ok(_) => "Couldn't make a layout report for this page.".to_owned(),
            Err(error) => format!("Couldn't make a layout report: {error:?}"),
        };
        crate::desktop::capture::set_notice(notice, Some(Duration::from_secs(15)));
    });
}

fn write(report: &str) -> std::io::Result<std::path::PathBuf> {
    // A report holds the page's address and text: never from an incognito window.
    if crate::desktop::incognito::active() {
        return Err(std::io::Error::other("not saved from an incognito window"));
    }
    let dir = crate::desktop::diagnostics::dir()
        .ok_or_else(|| std::io::Error::other("no folder for diagnostics"))?;
    std::fs::create_dir_all(&dir)?;
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|time| time.as_secs())
        .unwrap_or_default();
    let path = dir.join(format!("layout-report-{stamp}.txt"));
    std::fs::write(&path, report)?;
    Ok(path)
}
