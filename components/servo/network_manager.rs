/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::collections::HashSet;

use net_traits::blob_url_store::UrlWithBlobClaim;
use net_traits::request::{CredentialsMode, Destination, Referrer, RequestBuilder, RequestMode};
use net_traits::{CoreResourceMsg, FetchChannels, ResourceThreads};
use servo_base::id::WebViewId;
use servo_constellation_traits::EmbedderToConstellationMessage;
use servo_url::ImmutableOrigin;
use url::Url;

use crate::proxies::ConstellationProxy;

#[derive(Clone, Debug, PartialEq)]
pub struct CacheEntry {
    key: String,
}

impl CacheEntry {
    pub fn new(key: String) -> Self {
        Self { key }
    }

    pub fn key(&self) -> &str {
        &self.key
    }
}

/// Provides APIs for managing network-related state.
///
/// `NetworkManager` is responsible for data owned by the networking layer,
/// such as the HTTP cache. This data is not considered site data and is
/// therefore intentionally separate from `SiteDataManager`.
pub struct NetworkManager {
    public_resource_threads: ResourceThreads,
    private_resource_threads: ResourceThreads,
    constellation_proxy: ConstellationProxy,
}

impl NetworkManager {
    pub(crate) fn new(
        public_resource_threads: ResourceThreads,
        private_resource_threads: ResourceThreads,
        constellation_proxy: ConstellationProxy,
    ) -> Self {
        Self {
            public_resource_threads,
            private_resource_threads,
            constellation_proxy,
        }
    }

    /// Returns cache entries currently stored in the HTTP cache.
    ///
    /// The returned list contains one [`CacheEntry`] per unique cache key
    /// (URL) for which the networking layer currently maintains cached
    /// responses.
    ///
    /// Both public and private browsing contexts are included in the result.
    ///
    /// Note: The networking layer currently only implements an in-memory HTTP
    /// cache. Support for an on-disk cache is under development.
    pub fn cache_entries(&self) -> Vec<CacheEntry> {
        let public_entries = self.public_resource_threads.cache_entries();
        let private_entries = self.private_resource_threads.cache_entries();

        let unique_keys: HashSet<String> = public_entries
            .into_iter()
            .chain(private_entries)
            .map(|entry| entry.key)
            .collect();

        unique_keys.into_iter().map(CacheEntry::new).collect()
    }

    /// Clears the network (HTTP) cache.
    ///
    /// This removes all cached network responses maintained by the networking
    /// layer for both public and private browsing contexts.
    ///
    /// Note: The networking layer currently only implements an in-memory HTTP
    /// cache. Support for an on-disk cache is under development.
    pub fn clear_cache(&self) {
        self.public_resource_threads.clear_cache();
        self.private_resource_threads.clear_cache();
    }

    /// Download `url` to a file, e.g. to retry a download that failed. It is fetched with the
    /// user's cookies but no referrer, and the embedder is asked where to save it through
    /// [`crate::WebViewDelegate::request_download`] of the given [`crate::WebView`].
    pub fn download(&self, webview_id: WebViewId, url: Url) {
        let request = RequestBuilder::new(
            Some(webview_id),
            UrlWithBlobClaim::from_url_without_having_claimed_blob(url.into()),
            Referrer::NoReferrer,
        )
        .mode(RequestMode::Navigate)
        .destination(Destination::Document)
        .credentials_mode(CredentialsMode::Include)
        .use_url_credentials(true)
        .origin(ImmutableOrigin::new_opaque());
        let _ = self
            .public_resource_threads
            .core_thread
            .send(CoreResourceMsg::Fetch(request, FetchChannels::Download(None)));
    }

    /// Set the network online state. This controls the value of [`navigator.onLine`]
    /// and may fire `online` and `offline` events in web content.
    ///
    /// [`navigator.onLine`]: https://html.spec.whatwg.org/multipage/#dom-navigator-online
    pub fn set_online_state(&self, online: bool) {
        self.constellation_proxy
            .send(EmbedderToConstellationMessage::SetNetworkOnlineState(
                online,
            ));
    }
}
