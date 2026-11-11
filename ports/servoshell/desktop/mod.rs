/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Contains files specific to the servoshell app for Desktop systems.

mod accelerated_gl_media;
pub(crate) mod adblock;
pub(crate) mod app;
pub(crate) mod capture;
pub(crate) mod clipboard;
mod bookmarks;
pub(crate) mod cli;
pub(crate) mod compare;
pub(crate) mod translate;
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
pub(crate) mod incognito;
mod headless_window;
mod keyutils;
pub(crate) mod layout_report;
mod memory;
pub(crate) mod passwords;
pub(crate) mod panes;
mod protocols;
mod report;
pub(crate) mod scrollbar;
pub(crate) mod scrollbar_geometry;
pub(crate) mod session;
mod settings;
pub(crate) mod shutdown;
pub(crate) mod sidebar;
pub(crate) mod single_instance;
mod splash;
mod tracing;
mod updater;
#[cfg(target_os = "windows")]
mod vsync;
#[cfg(feature = "webxr")]
mod webxr;
mod workspaces;
mod zen_import;
