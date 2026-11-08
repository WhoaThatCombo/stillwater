/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Contains files specific to the servoshell app for Desktop systems.

mod accelerated_gl_media;
pub(crate) mod adblock;
pub(crate) mod app;
mod bookmarks;
pub(crate) mod cli;
mod command_bar;
pub(crate) mod dialog;
pub(crate) mod downloads;
pub(crate) mod diagnostics;
pub(crate) mod event_loop;
pub(crate) mod find_bar;
#[cfg(feature = "gamepad")]
pub(crate) mod gamepad;
pub mod geometry;
mod gui;
pub(crate) mod headed_window;
pub(crate) mod history;
mod headless_window;
mod keyutils;
mod memory;
pub(crate) mod passwords;
pub(crate) mod panes;
mod protocols;
mod report;
pub(crate) mod session;
mod settings;
mod sidebar;
mod single_instance;
mod tracing;
mod updater;
#[cfg(feature = "webxr")]
mod webxr;
mod workspaces;
mod zen_import;
