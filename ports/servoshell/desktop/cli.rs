/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::{env, panic};

use crate::desktop::app::App;
use crate::desktop::event_loop::ServoShellEventLoop;
use crate::desktop::{diagnostics, single_instance, updater};
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
    let (opts, mut preferences, servoshell_preferences) = match parse_command_line_arguments(&*args) {
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
    if single_instance &&
        single_instance::forward_to_running_instance(
            servoshell_preferences.url.as_deref().unwrap_or_default(),
        )
    {
        return;
    }

    // Turn on Servo's experimental web features: in tools/sitecheck they took usable sites
    // from 65 to 71 of 81 and fixed blank pages. STILLWATER_NO_EXPERIMENTAL opts out.
    if env::var_os("STILLWATER_NO_EXPERIMENTAL").is_none() {
        for name in crate::prefs::EXPERIMENTAL_PREFS {
            preferences.set_value(name, servo::PrefValue::Bool(true));
        }
    }

    // Keep a disk cache so revisiting sites doesn't re-download everything. Servo only
    // enables its HTTP disk cache when it's given a folder.
    if preferences.network_http_disk_cache.is_empty() &&
        let Some(dir) = dirs::data_local_dir()
    {
        preferences.network_http_disk_cache =
            dir.join("Stillwater").join("cache").to_string_lossy().into_owned();
    }

    diagnostics::init();
    diagnostics::log(format!(
        "Stillwater {} starting: exe={:?} url={:?} single_instance={single_instance}",
        updater::CURRENT_VERSION,
        env::current_exe().ok(),
        servoshell_preferences.url,
    ));

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
        if servoshell_preferences.webdriver_port.get().is_none() {
            updater::check_in_background(proxy);
        }
    }

    {
        let mut app = App::new(opts, preferences, servoshell_preferences, &event_loop);
        event_loop.run_app(&mut app);
    }

    crate::platform::deinit(clean_shutdown)
}
