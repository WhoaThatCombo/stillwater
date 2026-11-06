/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::sync::{Arc, RwLock};

use content_security_policy::Destination;
use embedder_traits::{GenericEmbedderProxy, WebResourceRequest, WebResourceResponseMsg};
use log::error;
use net_traits::http_status::HttpStatus;
use net_traits::request::Request;
use net_traits::response::{Response, ResponseBody};
use net_traits::{NetworkError, ResourceFetchTimingContainer};
use servo_base::id::WebViewId;

use crate::embedder::NetToEmbedderMsg;

/// What an embedder's [RequestFilter] decided about a request.
#[derive(Clone, Debug, PartialEq)]
pub enum RequestDecision {
    /// Fetch it normally, without asking the embedder's load_web_resource.
    Allow,
    /// Fail it with a network error (e.g. an ad or tracker).
    Block,
    /// Answer it with this body instead of fetching it (e.g. a harmless stand-in for an ad
    /// script that a page needs to exist).
    Respond { content_type: String, body: Vec<u8> },
    /// Hand it to the embedder's load_web_resource as usual.
    AskEmbedder,
}

/// Decides about requests on the network thread, before (and usually instead of) the round trip
/// to the embedder's main thread. Used by content blockers, which must be fast.
pub type RequestFilter =
    Arc<dyn Fn(&WebResourceRequest, Option<WebViewId>) -> RequestDecision + Send + Sync>;

static REQUEST_FILTER: RwLock<Option<RequestFilter>> = RwLock::new(None);

/// Install (or with None, remove) the process-wide request filter.
pub fn set_request_filter(filter: Option<RequestFilter>) {
    *REQUEST_FILTER.write().unwrap_or_else(|error| error.into_inner()) = filter;
}

#[derive(Clone)]
pub struct RequestInterceptor {
    embedder_proxy: GenericEmbedderProxy<NetToEmbedderMsg>,
}

impl RequestInterceptor {
    pub fn new(embedder_proxy: GenericEmbedderProxy<NetToEmbedderMsg>) -> RequestInterceptor {
        RequestInterceptor { embedder_proxy }
    }

    pub(crate) async fn intercept_request(
        &self,
        request: &mut Request,
        response: &mut Option<Response>,
        timing_info: &ResourceFetchTimingContainer,
    ) {
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let is_for_main_frame = matches!(request.destination, Destination::Document);
        let web_resource_request = WebResourceRequest {
            method: request.method.clone(),
            url: request.url(),
            headers: request.headers.clone(),
            destination: request.destination,
            referrer_url: request.referrer.to_url().cloned(),
            is_for_main_frame,
            is_redirect: request.redirect_count > 0,
        };

        let filter = REQUEST_FILTER
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        if let Some(filter) = filter {
            match filter(&web_resource_request, request.target_webview_id) {
                RequestDecision::Allow => return,
                RequestDecision::Block => {
                    *response = Some(Response::network_error(NetworkError::LoadCancelled));
                    return;
                },
                RequestDecision::Respond { content_type, body } => {
                    let mut stand_in = Response::new(request.url(), timing_info.inner().clone());
                    if let Ok(value) = http::HeaderValue::from_str(&content_type) {
                        stand_in.headers.insert(http::header::CONTENT_TYPE, value);
                    }
                    stand_in.headers.insert(
                        http::header::ACCESS_CONTROL_ALLOW_ORIGIN,
                        http::HeaderValue::from_static("*"),
                    );
                    *stand_in.body.lock() = ResponseBody::Done(body);
                    *response = Some(stand_in);
                    return;
                },
                RequestDecision::AskEmbedder => {},            }
        }

        self.embedder_proxy
            .send(NetToEmbedderMsg::WebResourceRequested(
                request.target_webview_id,
                web_resource_request,
                sender,
            ));

        // TODO: use done_chan and run in CoreResourceThreadPool.
        let mut accumulated_body = Vec::new();
        while let Some(message) = receiver.recv().await {
            match message {
                WebResourceResponseMsg::Start(webresource_response) => {
                    let timing = timing_info.inner().clone();
                    let mut response_override =
                        Response::new(webresource_response.url.into(), timing);
                    response_override.headers = webresource_response.headers;
                    response_override.status = HttpStatus::new(
                        webresource_response.status_code,
                        webresource_response.status_message,
                    );
                    *response = Some(response_override);
                },
                WebResourceResponseMsg::SendBodyData(data) => {
                    accumulated_body.push(data);
                },
                WebResourceResponseMsg::FinishLoad => {
                    if accumulated_body.is_empty() {
                        break;
                    }
                    let Some(response) = response.as_mut() else {
                        error!("Received unexpected FinishLoad message");
                        break;
                    };
                    *response.body.lock() =
                        ResponseBody::Done(accumulated_body.into_iter().flatten().collect());
                    break;
                },
                WebResourceResponseMsg::CancelLoad => {
                    *response = Some(Response::network_error(NetworkError::LoadCancelled));
                    break;
                },
                WebResourceResponseMsg::DoNotIntercept => break,
            }
        }
    }
}
