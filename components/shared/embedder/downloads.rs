/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Types shared between the network stack and the embedder for downloads: responses that
//! are saved to disk instead of being displayed.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use servo_url::ServoUrl;

/// Identifies one download for as long as Servo runs.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct DownloadId(pub u64);

impl DownloadId {
    /// A new, process-wide unique [`DownloadId`].
    pub fn next() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        DownloadId(NEXT.fetch_add(1, Ordering::Relaxed))
    }
}

/// What is known about a download when it starts, before any of it is saved.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DownloadInfo {
    pub id: DownloadId,
    /// The URL the data comes from, after redirects.
    pub url: ServoUrl,
    /// A file name without any directory parts, from `Content-Disposition`, the `download`
    /// attribute of the link, or the URL, in that order. It is not sanitized for any particular
    /// file system.
    pub suggested_filename: String,
    /// The essence of the response's `Content-Type`, if it had one.
    pub mime_type: Option<String>,
    /// The size of the (decoded) body, if the server said.
    pub total_bytes: Option<u64>,
}

/// The embedder's answer to a download, and its later requests to stop it.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum DownloadControl {
    /// Write the data to this file. It is created (or truncated) by Servo, and removed again if
    /// the download fails or is cancelled.
    Accept(PathBuf),
    /// Don't save the data, or stop saving it, and stop fetching it.
    Cancel,
}

/// Progress of an accepted download, reported to the embedder.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub enum DownloadUpdate {
    /// More data was written to the file. Sent a few times per second at most.
    Progress {
        received_bytes: u64,
        total_bytes: Option<u64>,
    },
    /// All the data was written and the file closed.
    Finished { received_bytes: u64 },
    /// The download stopped because of a network or file error; the file was removed.
    Failed(String),
    /// The download was cancelled and the file removed.
    Cancelled,
}
