/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Loads resources using a mapping from well-known shortcuts to resource: urls.
//! Recognized shortcuts:
//! - servo:default-user-agent
//! - servo:experimental-preferences
//! - servo:config
//! - servo:newtab
//! - servo:settings (Stillwater's settings page)
//! - servo:preferences

use std::future::Future;
use std::pin::Pin;

use headers::{ContentType, HeaderMapExt};
use servo::UserAgentPlatform;
use servo::protocol_handler::{
    DoneChannel, FetchContext, NetworkError, ProtocolHandler, Request, ResourceFetchTiming,
    Response, ResponseBody,
};

use crate::desktop::protocols::resource::ResourceProtocolHandler;
use crate::prefs::EXPERIMENTAL_PREFS;

#[derive(Default)]
pub struct ServoProtocolHandler {}

impl ProtocolHandler for ServoProtocolHandler {
    fn privileged_paths(&self) -> &'static [&'static str] {
        &["config", "preferences"]
    }

    fn is_fetchable(&self) -> bool {
        true
    }

    fn load(
        &self,
        request: &mut Request,
        done_chan: &mut DoneChannel,
        context: &FetchContext,
    ) -> Pin<Box<dyn Future<Output = Response> + Send>> {
        let url = request.current_url();
        // The browser's own pages can't be framed by, or reached through a redirect from,
        // web pages.
        if ResourceProtocolHandler::is_framed_or_redirected(request) {
            return Box::pin(std::future::ready(Response::network_error(
                NetworkError::ResourceLoadError("Not available to web pages".to_owned()),
            )));
        }

        match url.path() {
            // The Zen-style sidebar UI: `servo:sidebar/<file>`.
            path if path.starts_with("sidebar/") => {
                let path = format!("/{path}");
                ResourceProtocolHandler::response_for_path(request, done_chan, context, &path)
            },
            // Stillwater's settings page: `servo:settings` and its files `servo:settings/<file>`.
            "settings" => ResourceProtocolHandler::response_for_path(
                request,
                done_chan,
                context,
                "/settings/index.html",
            ),
            path if path.starts_with("settings/") => {
                let path = format!("/{path}");
                ResourceProtocolHandler::response_for_path(request, done_chan, context, &path)
            },
            "config" => ResourceProtocolHandler::response_for_path(
                request,
                done_chan,
                context,
                "/config.html",
            ),
            "newtab" => ResourceProtocolHandler::response_for_path(
                request,
                done_chan,
                context,
                "/newtab.html",
            ),

            "preferences" => ResourceProtocolHandler::response_for_path(
                request,
                done_chan,
                context,
                "/preferences.html",
            ),

            "license" => ResourceProtocolHandler::response_for_path(
                request,
                done_chan,
                context,
                "/license.html",
            ),

            "experimental-preferences" => {
                let pref_list = EXPERIMENTAL_PREFS
                    .iter()
                    .map(|pref| format!("\"{pref}\""))
                    .collect::<Vec<String>>()
                    .join(",");
                json_response(request, format!("[{pref_list}]"))
            },

            // The search URL template chosen in Stillwater's settings, for the New Tab page.
            "search-engine.js" => {
                let template = serde_json::to_string(&crate::desktop::settings::search_template())
                    .unwrap_or_else(|_| "null".into());
                script_response(request, format!("window.STILLWATER_SEARCH = {template};"))
            },

            "default-user-agent" => {
                let user_agent = UserAgentPlatform::default().to_user_agent_string();
                json_response(request, format!("\"{user_agent}\""))
            },

            _ => Box::pin(std::future::ready(Response::network_error(
                NetworkError::ResourceLoadError("Invalid shortcut".to_owned()),
            ))),
        }
    }
}

fn json_response(
    request: &Request,
    body: String,
) -> Pin<Box<dyn Future<Output = Response> + Send>> {
    let mut response = Response::new(
        request.current_url(),
        ResourceFetchTiming::new(request.timing_type()),
    );
    response.headers.typed_insert(ContentType::json());
    *response.body.lock() = ResponseBody::Done(body.into_bytes());
    Box::pin(std::future::ready(response))
}

fn script_response(
    request: &Request,
    body: String,
) -> Pin<Box<dyn Future<Output = Response> + Send>> {
    let mut response = Response::new(
        request.current_url(),
        ResourceFetchTiming::new(request.timing_type()),
    );
    response
        .headers
        .typed_insert(ContentType::from(mime_guess::mime::APPLICATION_JAVASCRIPT_UTF_8));
    *response.body.lock() = ResponseBody::Done(body.into_bytes());
    Box::pin(std::future::ready(response))
}