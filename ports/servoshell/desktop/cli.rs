/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::{env, panic};

use crate::desktop::app::App;
use crate::desktop::event_loop::ServoShellEventLoop;
use crate::desktop::{diagnostics, incognito, single_instance, updater};
use crate::panic_hook;
use crate::prefs::{ArgumentParsingResult, parse_command_line_arguments};

pub fn main() {
    crate::crash_handler::install();
    crate::init_crypto();

    // TODO: once log-panics is released, can this be replaced by
    // log_panics::init()?
    panic::set_hook(Box::new(panic_hook::panic_hook));

    // Skip the first argument, which is the binary name.
    let args: Vec<String> = env::args().skip(1).collect();
    // A helper that deletes an ended incognito session's files (see `incognito::finish`).
    if incognito::run_cleanup_helper(&args) {
        return;
    }
    let (mut opts, mut preferences, servoshell_preferences) = match parse_command_line_arguments(&*args) {
        #[cfg(not(feature = "multiprocess"))]
        ArgumentParsingResult::ContentProcess(_) => {
            log::error!("IPC not enabled, cannot use contentprocesses");
            return;
        },
        #[cfg(feature = "multiprocess")]
        ArgumentParsingResult::ContentProcess(token) => {
            return servo::run_content_process(token);
        },
        ArgumentParsingResult::ChromeProcess(opts, preferences, servoshell_preferences) => {
            (opts, preferences, servoshell_preferences)
        },
        ArgumentParsingResult::Exit => {
            std::process::exit(0);
        },
        ArgumentParsingResult::ErrorParsing => {
            std::process::exit(1);
        },
    };

    // Like a normal browser, hand the URL to an already-running Stillwater and exit.
    // STILLWATER_NO_SINGLE_INSTANCE lets a test or development copy run beside an installed one.
    let single_instance = !servoshell_preferences.headless &&
        servoshell_preferences.webdriver_port.get().is_none() &&
        env::var_os("STILLWATER_NO_SINGLE_INSTANCE").is_none();
    // An incognito launch joins the running incognito session instead (its own pipe).
    let incognito = servoshell_preferences.incognito;
    if single_instance && {
        let url = servoshell_preferences.url.as_deref().unwrap_or_default();
        if incognito {
            single_instance::forward_to_incognito_instance(url)
        } else {
            single_instance::forward_to_running_instance(url)
        }
    } {
        return;
    }

    // Turn on Servo's experimental web features: in tools/sitecheck they took usable sites
    // from 65 to 71 of 81 and fixed blank pages. STILLWATER_NO_EXPERIMENTAL opts out.
    // STILLWATER_DEFAULT_OFF_PREFS are left as they are (off unless set with --pref).
    if env::var_os("STILLWATER_NO_EXPERIMENTAL").is_none() {
        for name in crate::prefs::EXPERIMENTAL_PREFS
            .iter()
            .filter(|name| !crate::prefs::STILLWATER_DEFAULT_OFF_PREFS.contains(*name))
            .chain(crate::prefs::STILLWATER_EXTRA_PREFS)
        {
            preferences.set_value(name, servo::PrefValue::Bool(true));
        }
    }

    // Keep a disk cache so revisiting sites doesn't re-download everything. Servo only
    // enables its HTTP disk cache when it's given a folder.
    // A scratch STILLWATER_CONFIG_DIR profile keeps its cache to itself as well.
    if preferences.network_http_disk_cache.is_empty() {
        let scratch = env::var_os("STILLWATER_CONFIG_DIR")
            .filter(|dir| !dir.is_empty())
            .map(|dir| std::path::PathBuf::from(dir).join("cache"));
        if let Some(dir) =
            scratch.or_else(|| Some(dirs::data_local_dir()?.join("Stillwater").join("cache")))
        {
            preferences.network_http_disk_cache = dir.to_string_lossy().into_owned();
        }
    }

    // Incognito: a temporary engine folder and a memory-only HTTP cache. Without them it must
    // not run at all (it would use the normal profile).
    if let Err(error) = incognito::startup(incognito, single_instance, &args, &mut opts, &mut preferences) {
        eprintln!("Stillwater couldn't create a private folder for the incognito window: {error}");
        std::process::exit(1);
    }

    diagnostics::init();
    if incognito {
        diagnostics::log(format!(
            "incognito: session started (Stillwater {}, single_instance={single_instance})",
            updater::CURRENT_VERSION
        ));
    }
    diagnostics::log(format!(
        "Stillwater {} starting: exe={:?} url={:?} single_instance={single_instance}",
        updater::CURRENT_VERSION,
        env::current_exe().ok(),
        servoshell_preferences.url,
    ));
    for note in single_instance::take_notes() {
        diagnostics::log(format!("single instance: {note}"));
    }

    crate::init_tracing(servoshell_preferences.tracing_filter.as_deref());

    let clean_shutdown = servoshell_preferences.clean_shutdown;
    let event_loop = match servoshell_preferences.headless {
        true => ServoShellEventLoop::headless(),
        false => ServoShellEventLoop::headed(),
    };
    if let Some(proxy) = event_loop.event_loop_proxy() {
        if single_instance {
            single_instance::listen_for_other_instances(proxy.clone());
        }
        // Updates are left to the normal window.
        if servoshell_preferences.webdriver_port.get().is_none() && !incognito {
            updater::check_in_background(proxy);
        }
    }

    {
        let mut app = App::new(opts, preferences, servoshell_preferences, &event_loop);
        event_loop.run_app(&mut app);
    }
    diagnostics::log("shutdown: event loop finished; exiting");
    incognito::finish();

    crate::platform::deinit(clean_shutdown);
    // Everything that needs saving has been: the App (and with it Servo, the session writer,
    // the history writer and the other savers, which join their threads when dropped) was
    // dropped above. Returning from main now only runs ExitProcess's DLL detach for ~100
    // libraries (GStreamer, GLib, ANGLE, the GPU driver), which kept the process alive for
    // another 120-150 ms after this line (measured), holding the single-instance pipe while a
    // relaunch waits. `--clean-shutdown` keeps the old behaviour.
    if !clean_shutdown {
        crate::desktop::shutdown::exit_now();
    }
}
