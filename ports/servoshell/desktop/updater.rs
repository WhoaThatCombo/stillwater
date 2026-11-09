/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Stillwater's auto-updater. On launch it asks GitHub for the newest release of
//! WhoaThatCombo/stillwater (pre-releases included). If it's newer than this build, the
//! sidebar offers an update. Installing downloads `Stillwater-Setup*.exe`, checks it against
//! the release's `.sha256` file, quits Stillwater, runs the setup with `/passive` and starts
//! the updated browser again.
//!
//! Downloads use Windows' built-in `curl.exe` (Windows 10 1803 and later).

use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use log::warn;
use serde_json::Value;
use sha2::{Digest, Sha256};
use winit::event_loop::EventLoopProxy;

use crate::desktop::diagnostics;
use crate::desktop::event_loop::AppEvent;

/// The full path of a program that comes with Windows. A bare name would be looked up in
/// Stillwater's own folder first, where a downloaded file of that name could be waiting.
pub(crate) fn system_program(name: &str) -> std::path::PathBuf {
    let root = std::env::var_os("SystemRoot")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(r"C:\Windows"));
    let path = match name {
        "explorer.exe" => root.join(name),
        "powershell.exe" => root.join("System32/WindowsPowerShell/v1.0").join(name),
        _ => root.join("System32").join(name),
    };
    if path.is_file() { path } else { name.into() }
}

/// This build's Stillwater version. Keep in sync with `support/windows/*.wxs*` and the
/// `ProductVersion` in `ports/servoshell/Cargo.toml` when releasing.
pub(crate) const CURRENT_VERSION: &str = "0.8.2";

const RELEASES_API: &str =
    "https://api.github.com/repos/WhoaThatCombo/stillwater/releases?per_page=10";

#[derive(Clone, Debug)]
pub(crate) struct UpdateInfo {
    pub(crate) version: String,
    /// The release's web page, for portable copies that can't self-install.
    pub(crate) page_url: String,
    pub(crate) setup_url: Option<String>,
    pub(crate) checksum_url: Option<String>,
    /// Whether this copy was installed by the Stillwater setup (and so can update itself).
    pub(crate) installable: bool,
}

/// Where an update stands, as shown in the sidebar.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum UpdateStatus {
    Downloading,
    Installing,
    Failed(String),
}

fn parse_version(text: &str) -> Option<(u64, u64, u64)> {
    let mut parts = text.trim_start_matches('v').split(['.', '-']);
    Some((
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
    ))
}

fn curl() -> Command {
    let mut command = Command::new(crate::desktop::updater::system_program("curl.exe"));
    command.args([
        "--silent",
        "--show-error",
        "--fail",
        "--location",
        "--max-time",
        "300",
        "--user-agent",
        "Stillwater-Updater",
    ]);
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
}

fn fetch(url: &str) -> Result<Vec<u8>, String> {
    let output = curl()
        .arg(url)
        .output()
        .map_err(|error| format!("couldn't run curl.exe: {error}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_owned());
    }
    Ok(output.stdout)
}

/// True when running from the per-user install the Stillwater setup creates.
fn is_installed_copy() -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    let installed = exe
        .file_name()
        .is_some_and(|name| name.eq_ignore_ascii_case("Stillwater.exe")) &&
        exe.parent()
            .is_some_and(|dir| dir.ends_with(r"Programs\Stillwater"));
    installed || std::env::var_os("STILLWATER_UPDATE_TEST").is_some()
}

fn find_update(releases: &Value) -> Option<UpdateInfo> {
    // STILLWATER_PRETEND_VERSION lets testers exercise the update flow against real releases.
    let current = std::env::var("STILLWATER_PRETEND_VERSION")
        .ok()
        .and_then(|version| parse_version(&version))
        .or_else(|| parse_version(CURRENT_VERSION))?;
    let (version, release) = releases
        .as_array()?
        .iter()
        .filter(|release| !release["draft"].as_bool().unwrap_or(false))
        .filter_map(|release| {
            let version = parse_version(release["tag_name"].as_str()?)?;
            (version > current).then_some((version, release))
        })
        .max_by_key(|(version, _)| *version)?;

    let assets = release["assets"].as_array()?;
    let asset_url = |predicate: &dyn Fn(&str) -> bool| {
        assets
            .iter()
            .find(|asset| asset["name"].as_str().is_some_and(predicate))
            .and_then(|asset| asset["browser_download_url"].as_str())
            .map(str::to_owned)
    };
    let setup_url = asset_url(&|name| name.starts_with("Stillwater-Setup") && name.ends_with(".exe"));
    let checksum_url =
        asset_url(&|name| name.starts_with("Stillwater-Setup") && name.ends_with(".exe.sha256"));
    Some(UpdateInfo {
        version: format!("{}.{}.{}", version.0, version.1, version.2),
        page_url: release["html_url"].as_str()?.to_owned(),
        installable: is_installed_copy() && setup_url.is_some() && checksum_url.is_some(),
        setup_url,
        checksum_url,
    })
}

/// Check GitHub for a newer release in the background after launch; reports via
/// `AppEvent::UpdateAvailable`.
pub(crate) fn check_in_background(proxy: EventLoopProxy<AppEvent>) {
    if std::env::var_os("STILLWATER_NO_UPDATE_CHECK").is_some() {
        return;
    }
    check(proxy, Duration::from_secs(5), false);
}

/// Check right away (the settings panel's button); also reports "up to date" and failures
/// via `AppEvent::UpdateCheckDone`.
pub(crate) fn check_now(proxy: EventLoopProxy<AppEvent>) {
    check(proxy, Duration::ZERO, true);
}

fn check(proxy: EventLoopProxy<AppEvent>, delay: Duration, report: bool) {
    let _ = std::thread::Builder::new()
        .name("StillwaterUpdateCheck".into())
        .spawn(move || {
            // Don't compete with the first page load.
            std::thread::sleep(delay);
            diagnostics::log("update check: asking GitHub for releases");
            let releases = match fetch(RELEASES_API)
                .and_then(|body| serde_json::from_slice::<Value>(&body).map_err(|e| e.to_string()))
            {
                Ok(releases) => releases,
                Err(error) => {
                    warn!("Update check failed: {error}");
                    diagnostics::log(format!("update check failed: {error}"));
                    if report {
                        let _ = proxy.send_event(AppEvent::UpdateCheckDone(format!(
                            "Couldn't check for updates: {error}"
                        )));
                    }
                    return;
                },
            };
            let tags: Vec<&str> = releases
                .as_array()
                .map(|list| list.iter().filter_map(|r| r["tag_name"].as_str()).collect())
                .unwrap_or_default();
            match find_update(&releases) {
                Some(update) => {
                    diagnostics::log(format!("update check: found {update:?}"));
                    // STILLWATER_UPDATE_AUTOINSTALL: start installing at once (for testing).
                    if std::env::var_os("STILLWATER_UPDATE_AUTOINSTALL").is_some() {
                        install(update.clone(), proxy.clone());
                    }
                    let _ = proxy.send_event(AppEvent::UpdateAvailable(update));
                },
                None => {
                    diagnostics::log(format!(
                        "update check: no newer release (running {}, pretend {:?}, releases {tags:?})",
                        CURRENT_VERSION,
                        std::env::var("STILLWATER_PRETEND_VERSION").ok()
                    ));
                    if report {
                        let _ = proxy.send_event(AppEvent::UpdateCheckDone(format!(
                            "Stillwater {CURRENT_VERSION} is up to date"
                        )));
                    }
                },
            }
        });
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn download_and_verify(update: &UpdateInfo) -> Result<PathBuf, String> {
    let setup_url = update.setup_url.as_deref().ok_or("the release has no setup")?;
    let checksum_url = update
        .checksum_url
        .as_deref()
        .ok_or("the release has no checksum")?;

    let expected = String::from_utf8_lossy(&fetch(checksum_url)?)
        .split_whitespace()
        .next()
        .map(str::to_ascii_lowercase)
        .ok_or("the checksum file is empty")?;
    let setup = fetch(setup_url)?;
    if sha256_hex(&setup) != expected {
        return Err("the download didn't match its checksum".into());
    }

    let path = std::env::temp_dir().join(format!("Stillwater-Setup-{}.exe", update.version));
    std::fs::write(&path, setup).map_err(|error| format!("couldn't save the setup: {error}"))?;
    Ok(path)
}

/// Download, verify and launch the update. Reports progress with `AppEvent::UpdateStatus`
/// and sends `AppEvent::UpdateReadyToRestart` once the installer is waiting for us to quit.
pub(crate) fn install(update: UpdateInfo, proxy: EventLoopProxy<AppEvent>) {
    let _ = std::thread::Builder::new()
        .name("StillwaterUpdateInstall".into())
        .spawn(move || {
            let _ = proxy.send_event(AppEvent::UpdateStatus(UpdateStatus::Downloading));
            diagnostics::log(format!("update: downloading {:?}", update.setup_url));
            let setup = match download_and_verify(&update) {
                Ok(setup) => {
                    diagnostics::log(format!("update: verified {}", setup.display()));
                    setup
                },
                Err(error) => {
                    diagnostics::log(format!("update failed: {error}"));
                    let _ = proxy.send_event(AppEvent::UpdateStatus(UpdateStatus::Failed(error)));
                    return;
                },
            };
            // STILLWATER_UPDATE_DRY_RUN: stop after downloading and verifying (for testing).
            if std::env::var_os("STILLWATER_UPDATE_DRY_RUN").is_some() {
                diagnostics::log("update: dry run, not installing");
                return;
            }
            let Ok(exe) = std::env::current_exe() else {
                return;
            };
            let _ = proxy.send_event(AppEvent::UpdateStatus(UpdateStatus::Installing));

            // Wait for Stillwater to quit, run the setup with only a progress bar, then
            // start the updated browser.
            let quote = |path: &std::path::Path| path.display().to_string().replace('\'', "''");
            let script = format!(
                "Start-Sleep -Seconds 2; \
                 Start-Process -Wait -FilePath '{}' -ArgumentList '/passive','/norestart'; \
                 Start-Process -FilePath '{}'",
                quote(&setup),
                quote(&exe),
            );
            // STILLWATER_UPDATE_FAKE_INSTALL: run a harmless script instead of the setup (testing).
            let script = if std::env::var_os("STILLWATER_UPDATE_FAKE_INSTALL").is_some() {
                "Set-Content -Path (Join-Path $env:TEMP 'stillwater-fake-1.txt') -Value started; Start-Sleep -Seconds 2; Set-Content -Path (Join-Path $env:TEMP 'stillwater-fake-2.txt') -Value finished".to_owned()
            } else {
                script
            };
            diagnostics::log(format!("update: starting installer: {script}"));
            let mut command = Command::new(crate::desktop::updater::system_program("powershell.exe"));
            command.args([
                "-NoProfile",
                "-NonInteractive",
                "-WindowStyle",
                "Hidden",
                "-Command",
                &script,
            ]);
            #[cfg(target_os = "windows")]
            {
                use std::os::windows::process::CommandExt;
                // Hidden, in its own process group so it outlives Stillwater. Not DETACHED_PROCESS:
                // PowerShell started detached with inherited (invalid) stdio exits immediately.
                const CREATE_NO_WINDOW: u32 = 0x0800_0000;
                const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
                command.creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP);
            }
            command
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            match command.spawn() {
                Ok(_) => {
                    let _ = proxy.send_event(AppEvent::UpdateReadyToRestart);
                },
                Err(error) => {
                    diagnostics::log(format!("update: couldn't start the installer: {error}"));
                    let _ = proxy.send_event(AppEvent::UpdateStatus(UpdateStatus::Failed(
                        format!("couldn't start the installer: {error}"),
                    )));
                },
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_numerically() {
        assert!(parse_version("v0.10.0") > parse_version("v0.9.9"));
        assert_eq!(parse_version("v0.2.0"), Some((0, 2, 0)));
        assert_eq!(parse_version("0.2.0-beta"), Some((0, 2, 0)));
        assert_eq!(parse_version("nightly"), None);
    }

    #[test]
    fn sha256_is_lowercase_hex() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
