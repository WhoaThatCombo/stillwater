/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! The embedder's side of downloads: see [`crate::WebViewDelegate::request_download`].

use std::path::PathBuf;

use crossbeam_channel::Sender;
use embedder_traits::{DownloadControl, DownloadId, DownloadInfo};
use url::Url;

/// A response that should be saved as a file rather than displayed: a navigation to a file
/// with `Content-Disposition: attachment` or a type Servo can't display, or a click on an
/// `<a download>` link.
///
/// The data is already being fetched when this request arrives (through Servo's network
/// stack, with the page's cookies and credentials) and is held until the embedder answers,
/// so answer promptly: call [`DownloadRequest::accept`] with a file to write to, or
/// [`DownloadRequest::cancel`]. Dropping the request cancels the download.
pub struct DownloadRequest {
    info: DownloadInfo,
    control: Option<Sender<DownloadControl>>,
}

impl DownloadRequest {
    pub(crate) fn new(info: DownloadInfo, control: Sender<DownloadControl>) -> Self {
        Self {
            info,
            control: Some(control),
        }
    }

    /// The id that [`crate::ServoDelegate::notify_download_update`] reports progress with.
    pub fn id(&self) -> DownloadId {
        self.info.id
    }

    /// The URL the data comes from, after redirects.
    pub fn url(&self) -> Url {
        self.info.url.clone().into_url()
    }

    /// A file name suggested by the server or the page, without any directory. It is not
    /// sanitized for any file system.
    pub fn suggested_filename(&self) -> &str {
        &self.info.suggested_filename
    }

    /// The MIME type of the data (e.g. `application/zip`), if known.
    pub fn mime_type(&self) -> Option<&str> {
        self.info.mime_type.as_deref()
    }

    /// The size of the file in bytes, if known.
    pub fn total_bytes(&self) -> Option<u64> {
        self.info.total_bytes
    }

    /// Save the data to `path`. Servo creates (or replaces) the file, writes to it as the data
    /// arrives and removes it again if the download fails or is cancelled. Progress is reported
    /// to [`crate::ServoDelegate::notify_download_update`].
    pub fn accept(mut self, path: PathBuf) -> DownloadHandle {
        let control = self.control.take().expect("Request not answered yet");
        let _ = control.send(DownloadControl::Accept(path));
        DownloadHandle {
            id: self.info.id,
            control,
        }
    }

    /// Don't save the data, and stop fetching it.
    pub fn cancel(self) {
        // Cancelled when dropped.
    }
}

impl Drop for DownloadRequest {
    fn drop(&mut self) {
        if let Some(control) = self.control.take() {
            let _ = control.send(DownloadControl::Cancel);
        }
    }
}

/// A download that is being saved. It keeps going when this handle is dropped.
#[derive(Clone)]
pub struct DownloadHandle {
    id: DownloadId,
    control: Sender<DownloadControl>,
}

impl DownloadHandle {
    pub fn id(&self) -> DownloadId {
        self.id
    }

    /// Stop the download. Servo removes the partly written file and reports
    /// [`embedder_traits::DownloadUpdate::Cancelled`], unless the download already ended.
    pub fn cancel(&self) {
        let _ = self.control.send(DownloadControl::Cancel);
    }
}
