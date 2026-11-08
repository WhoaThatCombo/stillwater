/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Downloads: responses that are saved to a file instead of being displayed.
//!
//! A navigation's response becomes a download when it has `Content-Disposition: attachment`
//! or a type Servo can't display (see [`is_download`]). `<a download>` links start a fetch
//! that is always a download ([`FetchChannels::Download`]).
//!
//! The decision is made here, in the network stack, when the response headers arrive, so the
//! body is never sent to script and the same request (with its cookies, credentials, referrer
//! and POST body) is not repeated. [`DownloadInterceptor`] wraps the navigation's fetch target:
//!
//! - To the navigation it looks like a `204 No Content` response, so the navigation is aborted
//!   and the current document stays, which is what the HTML specification asks for.
//! - The embedder is asked where to save the file ([`NetToEmbedderMsg::RequestDownload`]). The
//!   body is streamed to a writer thread that writes it to the chosen file and reports progress
//!   ([`NetToEmbedderMsg::DownloadUpdate`]), so large files are never held in memory.
//!
//! A download outlives the document and the `WebView` that started it.
//!
//! [`FetchChannels::Download`]: net_traits::FetchChannels::Download

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use bytes::Bytes;
use content_security_policy as csp;
use crossbeam_channel::{Receiver, Sender, never, select, unbounded};
use embedder_traits::{
    DownloadControl, DownloadId, DownloadInfo, DownloadUpdate, GenericEmbedderProxy,
};
use http::{HeaderMap, StatusCode, header};
use log::{debug, warn};
use mime::Mime;
use net_traits::mime_classifier::MimeClassifier;
use net_traits::request::{Destination, Request, RequestBuilder, RequestMode};
use net_traits::response::{Response, ResponseBody};
use net_traits::{FetchTaskTarget, ResourceFetchTiming, ResourceTimingType};
use parking_lot::Mutex;

use crate::embedder::NetToEmbedderMsg;
use crate::fetch::methods::CancellationListener;

/// The embedder hears about a download's progress at most this often.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(250);

/// Whether a navigation response should be saved instead of displayed.
fn is_download(request: &Request, response: &Response) -> bool {
    if response.is_network_error() {
        return false;
    }
    let response = response.actual_response();
    // Redirects are followed by the navigation itself; 204 and 205 already leave the page as
    // it is; error pages are displayed.
    if !response.status.is_success() || response.status.in_range(204..=205) {
        return false;
    }
    // Like other browsers, don't let a frame download without user activation (e.g. ads).
    if request.destination != Destination::Document && !request.user_activation {
        return false;
    }
    if is_attachment(&response.headers) {
        return true;
    }
    let Some(mime) = content_type(&response.headers) else {
        // No type: the navigation sniffs the content.
        return false;
    };
    // Local files without a recognisable extension are reported as `application/octet-stream`
    // and sniffed instead.
    let is_file = response.url().is_some_and(|url| url.scheme() == "file");
    if is_file && mime.essence_str() == "application/octet-stream" {
        return false;
    }
    is_undisplayable_type(&mime)
}

/// Types the navigation can't display: binary `application/` types (archives, programs,
/// documents such as PDF), except the ones Servo renders (XML, JSON, JavaScript and so on).
fn is_undisplayable_type(mime: &Mime) -> bool {
    if mime.essence_str() == "binary/octet-stream" {
        return true;
    }
    mime.type_() == mime::APPLICATION &&
        // "application/unknown" asks for sniffing.
        mime.subtype().as_str() != "unknown" &&
        MimeClassifier::get_media_type(mime).is_none()
}

fn content_type(headers: &HeaderMap) -> Option<Mime> {
    headers
        .get(header::CONTENT_TYPE)?
        .to_str()
        .ok()?
        .parse()
        .ok()
}

fn is_attachment(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_DISPOSITION)
        .map(|value| String::from_utf8_lossy(value.as_bytes()).into_owned())
        .is_some_and(|value| {
            value
                .split(';')
                .next()
                .is_some_and(|kind| kind.trim().eq_ignore_ascii_case("attachment"))
        })
}

/// The `filename*` (RFC 8187) or `filename` parameter of a `Content-Disposition` header.
fn filename_from_content_disposition(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(header::CONTENT_DISPOSITION)?.as_bytes();
    // Some servers send raw UTF-8 in `filename`; anything else is treated as Latin-1.
    let value = match std::str::from_utf8(value) {
        Ok(value) => value.to_owned(),
        Err(_) => value.iter().map(|byte| *byte as char).collect(),
    };
    let mut filename = None;
    for (name, value) in disposition_parameters(&value) {
        if name.eq_ignore_ascii_case("filename*") {
            // charset'language'percent-encoded-value
            let mut parts = value.splitn(3, '\'');
            let (Some(charset), Some(_language), Some(encoded)) =
                (parts.next(), parts.next(), parts.next())
            else {
                continue;
            };
            let bytes = percent_decode(encoded);
            let decoded = if charset.eq_ignore_ascii_case("utf-8") {
                String::from_utf8(bytes).ok()
            } else {
                Some(bytes.iter().map(|byte| *byte as char).collect())
            };
            if let Some(decoded) = decoded.filter(|name| !name.is_empty()) {
                // `filename*` wins over `filename`.
                return Some(decoded);
            }
        } else if name.eq_ignore_ascii_case("filename") && filename.is_none() {
            filename = Some(value).filter(|name| !name.is_empty());
        }
    }
    filename
}

/// The `name=value` parameters after the disposition type, with quoted values unquoted.
fn disposition_parameters(header: &str) -> Vec<(String, String)> {
    let mut parameters = Vec::new();
    let mut rest = match header.find(';') {
        Some(index) => &header[index + 1..],
        None => return parameters,
    };
    loop {
        rest = rest.trim_start_matches(|c: char| c == ';' || c.is_whitespace());
        let Some(equals) = rest.find('=') else {
            break;
        };
        let name = rest[..equals].trim().to_owned();
        rest = rest[equals + 1..].trim_start();
        let value;
        if let Some(quoted) = rest.strip_prefix('"') {
            let mut unquoted = String::new();
            let mut chars = quoted.char_indices();
            let mut end = quoted.len();
            while let Some((index, c)) = chars.next() {
                match c {
                    '\\' => {
                        if let Some((_, escaped)) = chars.next() {
                            unquoted.push(escaped);
                        }
                    },
                    '"' => {
                        end = index + 1;
                        break;
                    },
                    c => unquoted.push(c),
                }
            }
            value = unquoted;
            rest = &quoted[end.min(quoted.len())..];
        } else {
            let end = rest.find(';').unwrap_or(rest.len());
            value = rest[..end].trim().to_owned();
            rest = &rest[end..];
        }
        parameters.push((name, value));
    }
    parameters
}

fn percent_decode(text: &str) -> Vec<u8> {
    let bytes = text.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' &&
            let Some(byte) = text
                .get(index + 1..index + 3)
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
        {
            decoded.push(byte);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    decoded
}

/// The name to suggest for a download: from `Content-Disposition`, else the `download`
/// attribute, else the last part of the URL's path. Directory parts are removed.
fn suggested_filename(response: &Response, link_filename: Option<&str>) -> String {
    let from_url = || {
        let url = response.url()?;
        if !matches!(url.scheme(), "http" | "https" | "file" | "ftp") {
            return None;
        }
        let segment = url.as_url().path_segments()?.next_back()?;
        String::from_utf8(percent_decode(segment)).ok()
    };
    let name = filename_from_content_disposition(&response.headers)
        .or_else(|| link_filename.map(str::to_owned))
        .or_else(from_url)
        .unwrap_or_default();
    let name = name.rsplit(['/', '\\']).next().unwrap_or_default().trim();
    if name.is_empty() || name == "." || name == ".." {
        "download".to_owned()
    } else {
        name.to_owned()
    }
}

/// The body's size, if the server sent it and the body isn't compressed in transit (the
/// file gets the decoded bytes).
fn total_bytes(headers: &HeaderMap) -> Option<u64> {
    let encoded = headers
        .get(header::CONTENT_ENCODING)
        .is_some_and(|value| value.as_bytes() != b"identity");
    if encoded {
        return None;
    }
    headers
        .get(header::CONTENT_LENGTH)?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()
}

enum State {
    /// The response headers haven't arrived yet.
    Undecided,
    /// Not a download (or not a navigation): everything goes to the target.
    PassThrough,
    /// The body goes to the writer thread. `body` is the response's in-memory copy of the
    /// body, kept for the HTTP cache, which is emptied as data arrives.
    Downloading {
        writer: Sender<WriterMessage>,
        body: servo_arc::Arc<Mutex<ResponseBody>>,
    },
    /// A download that couldn't start (e.g. a link to an error page): ignore the rest.
    Ignored,
}

/// A fetch target that turns the response into a download when it should be one, and
/// otherwise hands everything to the navigation's own target.
pub(crate) struct DownloadInterceptor<Target> {
    /// The fetch's own target, or `None` for fetches that are always downloads.
    navigation: Option<Target>,
    /// The `download` attribute of the link that started a download fetch.
    link_filename: Option<String>,
    embedder_proxy: GenericEmbedderProxy<NetToEmbedderMsg>,
    cancellation_listener: Arc<CancellationListener>,
    state: State,
}

impl<Target: FetchTaskTarget> DownloadInterceptor<Target> {
    /// Wrap a fetch's target. If the fetch is a navigation, its response is watched and
    /// saved as a download if it should be one; anything else passes straight through.
    pub(crate) fn new(
        target: Target,
        request: &RequestBuilder,
        embedder_proxy: GenericEmbedderProxy<NetToEmbedderMsg>,
        cancellation_listener: Arc<CancellationListener>,
    ) -> Self {
        let may_download = request.mode == RequestMode::Navigate;
        Self {
            navigation: Some(target),
            link_filename: None,
            embedder_proxy,
            cancellation_listener,
            state: if may_download {
                State::Undecided
            } else {
                State::PassThrough
            },
        }
    }

    /// Send `target`'s methods to the navigation, if there is one and the response isn't
    /// a download.
    fn navigation(&mut self) -> Option<&mut Target> {
        match self.state {
            State::Undecided | State::PassThrough => self.navigation.as_mut(),
            State::Downloading { .. } | State::Ignored => None,
        }
    }

    fn start_download(&mut self, request: &Request, response: &Response) {
        let actual = response.actual_response();
        let info = DownloadInfo {
            id: DownloadId::next(),
            url: actual
                .url()
                .cloned()
                .unwrap_or_else(|| request.current_url()),
            suggested_filename: suggested_filename(actual, self.link_filename.as_deref()),
            mime_type: content_type(&actual.headers).map(|mime| mime.essence_str().to_owned()),
            total_bytes: total_bytes(&actual.headers),
        };
        debug!("Download {:?} of {}", info.id, info.url);

        // The HTTP cache keeps the in-memory body of responses it stores. Mark this response
        // as aborted so the cache never answers from its (soon emptied) body, and drop what has
        // been buffered so far: from now on the body only goes to the file.
        actual.aborted.store(true, Ordering::Release);
        let body = actual.body.clone();
        empty_body(&body, true);

        let (writer, writer_receiver) = unbounded();
        let (control_sender, control_receiver) = unbounded();
        let embedder_proxy = self.embedder_proxy.clone();
        let cancellation_listener = self.cancellation_listener.clone();
        let thread_info = info.clone();
        let spawned = std::thread::Builder::new()
            .name("Download".to_owned())
            .spawn(move || {
                DownloadWriter {
                    info: thread_info,
                    embedder_proxy,
                    cancellation_listener,
                }
                .run(control_receiver, writer_receiver)
            });
        if let Err(error) = spawned {
            warn!("Could not start a download thread: {error}");
            self.cancellation_listener.cancel();
            self.state = State::Ignored;
            return;
        }
        self.embedder_proxy.send(NetToEmbedderMsg::RequestDownload(
            request.target_webview_id,
            info,
            control_sender,
        ));
        self.state = State::Downloading { writer, body };
    }
}

impl DownloadInterceptor<net_traits::DiscardFetch> {
    /// A fetch that is saved as a download whatever its type (`<a download>`).
    pub(crate) fn for_download(
        link_filename: Option<String>,
        embedder_proxy: GenericEmbedderProxy<NetToEmbedderMsg>,
        cancellation_listener: Arc<CancellationListener>,
    ) -> Self {
        Self {
            navigation: None,
            link_filename: link_filename.filter(|name| !name.trim().is_empty()),
            embedder_proxy,
            cancellation_listener,
            state: State::Undecided,
        }
    }
}

impl<Target: FetchTaskTarget> FetchTaskTarget for DownloadInterceptor<Target> {
    fn process_request_body(&mut self, request: &Request) {
        if let Some(navigation) = self.navigation() {
            navigation.process_request_body(request);
        }
    }

    fn process_response(&mut self, request: &Request, response: &Response) {
        if !matches!(self.state, State::Undecided) {
            if let Some(navigation) = self.navigation() {
                navigation.process_response(request, response);
            }
            return;
        }
        let Some(navigation) = self.navigation.as_mut() else {
            // A download fetch: save anything but errors.
            let actual = response.actual_response();
            if response.is_network_error() || !actual.status.is_success() {
                warn!(
                    "Download of {} failed: {:?}",
                    request.current_url(),
                    response.get_network_error().cloned().map_or_else(
                        || format!("HTTP {}", actual.status.raw_code()),
                        |error| format!("{error:?}")
                    )
                );
                self.state = State::Ignored;
            } else {
                self.start_download(request, response);
            }
            return;
        };
        if !is_download(request, response) {
            self.state = State::PassThrough;
            navigation.process_response(request, response);
            return;
        }
        // To the navigation, a download looks like "204 No Content": the navigation is
        // aborted and the current document stays.
        let url = response
            .actual_response()
            .url()
            .cloned()
            .unwrap_or_else(|| request.current_url());
        let mut no_content = Response::new(
            url,
            ResourceFetchTiming::new(ResourceTimingType::Navigation),
        );
        no_content.status = StatusCode::NO_CONTENT.into();
        no_content.url_list = response.actual_response().url_list.clone();
        navigation.process_response(request, &no_content);
        navigation.process_response_eof(request, &no_content);
        self.start_download(request, response);
    }

    fn process_response_chunk(&mut self, request: &Request, chunk: Bytes) {
        match &self.state {
            State::Downloading { writer, body } => {
                empty_body(body, false);
                let _ = writer.send(WriterMessage::Data(chunk));
            },
            State::Ignored => {},
            State::Undecided | State::PassThrough => {
                if let Some(navigation) = self.navigation.as_mut() {
                    navigation.process_response_chunk(request, chunk);
                }
            },
        }
    }

    fn process_response_eof(&mut self, request: &Request, response: &Response) {
        match &self.state {
            State::Downloading { writer, .. } => {
                let result = match response.get_network_error() {
                    Some(error) => Err(format!("{error:?}")),
                    None => Ok(()),
                };
                let _ = writer.send(WriterMessage::End(result));
            },
            State::Ignored => {},
            State::Undecided | State::PassThrough => {
                if let Some(navigation) = self.navigation.as_mut() {
                    navigation.process_response_eof(request, response);
                }
            },
        }
    }

    fn process_csp_violations(&mut self, request: &Request, violations: Vec<csp::Violation>) {
        if let Some(navigation) = self.navigation() {
            navigation.process_csp_violations(request, violations);
        }
    }

    fn process_response_length_hint(&mut self, request: &Request, length: usize) {
        if let Some(navigation) = self.navigation() {
            navigation.process_response_length_hint(request, length);
        }
    }
}

/// Drop the in-memory copy of a body that is still arriving from the network. Bodies that
/// arrived all at once (`data:`, files, the cache) are left alone: their callers hold the lock
/// while handing the data over, so only try to take it.
fn empty_body(body: &servo_arc::Arc<Mutex<ResponseBody>>, release_memory: bool) {
    if let Some(mut body) = body.try_lock() &&
        let ResponseBody::Receiving(ref mut data) = *body
    {
        if release_memory {
            *data = Vec::new();
        } else {
            data.clear();
        }
    }
}

enum WriterMessage {
    Data(Bytes),
    /// The body ended, or the fetch failed.
    End(Result<(), String>),
}

/// Writes one download's data to its file, on its own thread so slow disks never hold up
/// the network.
struct DownloadWriter {
    info: DownloadInfo,
    embedder_proxy: GenericEmbedderProxy<NetToEmbedderMsg>,
    cancellation_listener: Arc<CancellationListener>,
}

impl DownloadWriter {
    fn run(self, control: Receiver<DownloadControl>, data: Receiver<WriterMessage>) {
        // Wait for the embedder to choose a file. The data received meanwhile waits in the
        // channel, so embedders should answer promptly.
        let path = match control.recv() {
            Ok(DownloadControl::Accept(path)) => path,
            Ok(DownloadControl::Cancel) | Err(_) => {
                debug!("Download {:?} declined", self.info.id);
                self.cancellation_listener.cancel();
                return;
            },
        };
        let update = match self.write(&path, control, data) {
            Ok(received_bytes) => DownloadUpdate::Finished { received_bytes },
            Err(update) => {
                self.cancellation_listener.cancel();
                if let Err(error) = std::fs::remove_file(&path) &&
                    error.kind() != std::io::ErrorKind::NotFound
                {
                    warn!("Could not remove {}: {error}", path.display());
                }
                update
            },
        };
        self.send(update);
    }

    fn send(&self, update: DownloadUpdate) {
        self.embedder_proxy
            .send(NetToEmbedderMsg::DownloadUpdate(self.info.id, update));
    }

    /// Write the data to `path`, returning how many bytes were written, or the update to
    /// report if the download fails or is cancelled.
    fn write(
        &self,
        path: &Path,
        control: Receiver<DownloadControl>,
        data: Receiver<WriterMessage>,
    ) -> Result<u64, DownloadUpdate> {
        let failed = |error: std::io::Error| {
            DownloadUpdate::Failed(format!("Could not write {}: {error}", path.display()))
        };
        let mut file = BufWriter::with_capacity(256 * 1024, File::create(path).map_err(failed)?);
        let mut control = control;
        let mut received_bytes = 0u64;
        let mut last_progress = Instant::now();
        loop {
            select! {
                recv(control) -> message => match message {
                    Ok(DownloadControl::Cancel) => return Err(DownloadUpdate::Cancelled),
                    Ok(DownloadControl::Accept(_)) => {},
                    // The embedder no longer holds a handle: carry on.
                    Err(_) => control = never(),
                },
                recv(data) -> message => match message {
                    Ok(WriterMessage::Data(chunk)) => {
                        file.write_all(&chunk).map_err(failed)?;
                        received_bytes += chunk.len() as u64;
                        if last_progress.elapsed() >= PROGRESS_INTERVAL {
                            last_progress = Instant::now();
                            self.send(DownloadUpdate::Progress {
                                received_bytes,
                                total_bytes: self.info.total_bytes,
                            });
                        }
                    },
                    Ok(WriterMessage::End(Ok(()))) => break,
                    Ok(WriterMessage::End(Err(error))) => {
                        return Err(DownloadUpdate::Failed(network_error_text(&error)));
                    },
                    // The fetch went away without finishing (e.g. Servo is shutting down).
                    Err(_) => return Err(DownloadUpdate::Failed(
                        "The download was interrupted.".to_owned(),
                    )),
                },
            }
        }
        // A connection that closes early ends the body without an error.
        if self
            .info
            .total_bytes
            .is_some_and(|total| received_bytes < total)
        {
            return Err(DownloadUpdate::Failed(
                "The connection closed before the whole file arrived.".to_owned(),
            ));
        }
        file.flush().map_err(failed)?;
        file.into_inner()
            .map_err(|error| failed(error.into_error()))?
            .sync_all()
            .map_err(failed)?;
        Ok(received_bytes)
    }
}

fn network_error_text(error: &str) -> String {
    format!("Network error: {error}")
}

#[cfg(test)]
mod tests {
    use http::HeaderValue;

    use super::*;

    fn disposition(value: &[u8]) -> Option<String> {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONTENT_DISPOSITION,
            HeaderValue::from_bytes(value).unwrap(),
        );
        filename_from_content_disposition(&headers)
    }

    #[test]
    fn content_disposition_filenames() {
        assert_eq!(
            disposition(b"attachment; filename=\"a b.zip\""),
            Some("a b.zip".into())
        );
        assert_eq!(
            disposition(b"attachment; filename=plain.txt"),
            Some("plain.txt".into())
        );
        assert_eq!(
            disposition(
                b"attachment; filename=\"fallback.txt\"; filename*=UTF-8''%E2%82%AC%20rates.txt"
            ),
            Some("€ rates.txt".into())
        );
        assert_eq!(
            disposition(b"attachment; filename*=utf-8'en'na%C3%AFve.pdf"),
            Some("naïve.pdf".into())
        );
        assert_eq!(
            disposition(b"attachment; filename=\"quote\\\"d.txt\""),
            Some("quote\"d.txt".into())
        );
        assert_eq!(disposition(b"attachment"), None);
        assert_eq!(
            disposition("attachment; filename=\"日本.txt\"".as_bytes()),
            Some("日本.txt".into())
        );
    }

    #[test]
    fn undisplayable_types() {
        let undisplayable = |mime: &str| is_undisplayable_type(&mime.parse().unwrap());
        assert!(undisplayable("application/octet-stream"));
        assert!(undisplayable("application/zip"));
        assert!(undisplayable("application/pdf"));
        assert!(undisplayable("application/x-msdownload"));
        assert!(!undisplayable("application/json"));
        assert!(!undisplayable("application/ld+json"));
        assert!(!undisplayable("application/xhtml+xml"));
        assert!(!undisplayable("application/javascript"));
        assert!(!undisplayable("application/ogg"));
        assert!(!undisplayable("application/unknown"));
        assert!(!undisplayable("text/plain"));
        assert!(!undisplayable("image/png"));
        assert!(!undisplayable("video/mp4"));
        assert!(!undisplayable("multipart/x-mixed-replace"));
    }
}
