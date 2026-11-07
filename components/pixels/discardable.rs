/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Stillwater: decoded image pixels that can be thrown away and decoded again.
//!
//! A decoded image takes width × height × 4 bytes, often ten times its file size, and Servo
//! used to keep the pixels of every image a page had loaded for as long as the page lived: in
//! the image cache, in the elements showing it, and again as WebRender's copy. Pages with long
//! image feeds ran into gigabytes.
//!
//! Large still images now keep their encoded (compressed) bytes, and their pixels live in a
//! process-wide store with a budget. When the store is over budget, the pixels of the images
//! used least recently are dropped. Whoever needs them again (WebRender uploading a texture,
//! a canvas drawing the image) gets them through [`ImageBytes::get`], which decodes again if
//! needed. WebRender sees these images as external buffer images (see
//! [`ImageBytes::external_image_data`] and [`lock_external_image`]), so it keeps no copy of
//! its own: it asks for the pixels only when it uploads the image to its texture cache.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex, RwLock, Weak};
use std::time::{Duration, Instant};

use log::{debug, warn};
use malloc_size_of::{MallocConditionalSizeOf, MallocSizeOf, MallocSizeOfOps};
use servo_base::generic_channel::GenericSharedMemory;
use webrender_api::{ExternalImageData, ExternalImageId, ExternalImageType};

use crate::CorsStatus;

/// External image ids with this bit set belong to [`ImageBytes`] (the rest come from Servo's
/// WebGL, media and WebGPU external images, which count up from 1).
pub const DISCARDABLE_IMAGE_ID_BIT: u64 = 1 << 62;

/// How much decoded pixel data the store keeps before dropping some.
const DECODED_BUDGET: usize = 256 * 1024 * 1024;
/// When over budget, drop down to this.
const DECODED_TARGET: usize = 192 * 1024 * 1024;
/// Pixels used more recently than this are never dropped (they're likely being uploaded or
/// drawn right now).
const MIN_IDLE: Duration = Duration::from_secs(3);
/// Images smaller than this aren't worth the bookkeeping.
const MIN_DISCARDABLE_BYTES: usize = 128 * 1024;

static ENABLED: AtomicBool = AtomicBool::new(false);
static NEXT_ID: AtomicU64 = AtomicU64::new(1);
static DECODED_TOTAL: AtomicUsize = AtomicUsize::new(0);
/// When [`discard_if_over_budget`] last looked for pixels to drop, in ms since [`EPOCH`].
static LAST_SCAN: AtomicU64 = AtomicU64::new(0);
static EPOCH: LazyLock<Instant> = LazyLock::new(Instant::now);
/// Every discardable image, for finding the least recently used ones.
static ALL: LazyLock<Mutex<Vec<Weak<Inner>>>> = LazyLock::new(Default::default);
/// External image id to image, for WebRender's lock calls.
static BY_ID: LazyLock<RwLock<rustc_hash::FxHashMap<u64, Weak<Inner>>>> =
    LazyLock::new(Default::default);

/// Turn discardable images on. Only valid when WebRender runs in this process (the external
/// image ids are looked up in this process's store).
pub fn set_discardable_images_enabled(enabled: bool) {
    let enabled = enabled && std::env::var_os("STILLWATER_KEEP_DECODED_IMAGES").is_none();
    ENABLED.store(enabled, Ordering::Relaxed);
}

fn now_ms() -> u64 {
    EPOCH.elapsed().as_millis() as u64
}

pub struct Inner {
    id: u64,
    len: usize,
    encoded: Box<[u8]>,
    cors_status: CorsStatus,
    decoded: Mutex<Option<Arc<Vec<u8>>>>,
    last_used: AtomicU64,
}

impl Inner {
    fn get(&self) -> Arc<Vec<u8>> {
        self.last_used.store(now_ms(), Ordering::Relaxed);
        let mut decoded = self.decoded.lock().unwrap();
        if let Some(bytes) = decoded.as_ref() {
            let bytes = bytes.clone();
            drop(decoded);
            // Loads come in bursts; trim once they're no longer recent.
            discard_if_over_budget();
            return bytes;
        }
        let bytes = match crate::load_from_memory(&self.encoded, self.cors_status) {
            Some(image) if image.bytes.len() == self.len => image.bytes.get(),
            _ => {
                warn!("Couldn't decode a discarded image again");
                Arc::new(vec![0; self.len])
            },
        };
        debug!("Decoded discarded image {} again ({} bytes)", self.id, self.len);
        *decoded = Some(bytes.clone());
        drop(decoded);
        DECODED_TOTAL.fetch_add(self.len, Ordering::Relaxed);
        discard_if_over_budget();
        bytes
    }

    fn discard(&self) -> bool {
        let mut decoded = self.decoded.lock().unwrap();
        if decoded.take().is_some() {
            DECODED_TOTAL.fetch_sub(self.len, Ordering::Relaxed);
            return true;
        }
        false
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        if self.decoded.get_mut().unwrap().is_some() {
            DECODED_TOTAL.fetch_sub(self.len, Ordering::Relaxed);
        }
        BY_ID.write().unwrap().remove(&self.id);
    }
}

/// Drop the pixels of the least recently used images until the store is back under budget.
fn discard_if_over_budget() {
    if DECODED_TOTAL.load(Ordering::Relaxed) <= DECODED_BUDGET {
        return;
    }
    let now = now_ms();
    let last_scan = LAST_SCAN.load(Ordering::Relaxed);
    if now.saturating_sub(last_scan) < 500 ||
        LAST_SCAN
            .compare_exchange(last_scan, now, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
    {
        return;
    }
    let mut candidates: Vec<(u64, Arc<Inner>)> = {
        let mut all = ALL.lock().unwrap();
        all.retain(|weak| weak.strong_count() > 0);
        all.iter()
            .filter_map(Weak::upgrade)
            .map(|inner| (inner.last_used.load(Ordering::Relaxed), inner))
            .filter(|(last_used, _)| now.saturating_sub(*last_used) >= MIN_IDLE.as_millis() as u64)
            .collect()
    };
    candidates.sort_unstable_by_key(|(last_used, _)| *last_used);
    let mut dropped = 0;
    for (_, inner) in candidates {
        if DECODED_TOTAL.load(Ordering::Relaxed) <= DECODED_TARGET {
            break;
        }
        if inner.discard() {
            dropped += 1;
        }
    }
    debug!(
        "Dropped the pixels of {dropped} images; {} MB decoded",
        DECODED_TOTAL.load(Ordering::Relaxed) >> 20
    );
}

/// The pixels of a [`crate::RasterImage`].
#[derive(Clone)]
pub enum ImageBytes {
    /// Always in memory.
    Plain(Arc<Vec<u8>>),
    /// Can be dropped and decoded again from the encoded bytes.
    Discardable(Arc<Inner>),
}

impl ImageBytes {
    pub fn new(bytes: Arc<Vec<u8>>) -> Self {
        ImageBytes::Plain(bytes)
    }

    /// The pixels, decoding the image again if they were dropped. Hold on to the result only
    /// as long as needed: while it's held, dropping the pixels frees nothing.
    pub fn get(&self) -> Arc<Vec<u8>> {
        match self {
            ImageBytes::Plain(bytes) => bytes.clone(),
            ImageBytes::Discardable(inner) => inner.get(),
        }
    }

    pub fn len(&self) -> usize {
        match self {
            ImageBytes::Plain(bytes) => bytes.len(),
            ImageBytes::Discardable(inner) => inner.len,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// For WebRender: discardable images are external buffer images, whose pixels WebRender
    /// asks for (through [`lock_external_image`]) when it uploads them.
    pub fn external_image_data(&self) -> Option<ExternalImageData> {
        match self {
            ImageBytes::Plain(_) => None,
            ImageBytes::Discardable(inner) => Some(ExternalImageData {
                id: ExternalImageId(inner.id),
                channel_index: 0,
                image_type: ExternalImageType::Buffer,
                normalized_uvs: false,
            }),
        }
    }

    pub fn to_shared_memory(&self) -> GenericSharedMemory {
        GenericSharedMemory::from_arc_vec(self.get())
    }

    /// Make these pixels discardable, keeping `encoded` to decode them again. Does nothing if
    /// discardable images are off or the image is small.
    pub(crate) fn make_discardable(&mut self, encoded: &[u8], cors_status: CorsStatus) {
        let ImageBytes::Plain(bytes) = self else {
            return;
        };
        if !ENABLED.load(Ordering::Relaxed) || bytes.len() < MIN_DISCARDABLE_BYTES {
            return;
        }
        let id = DISCARDABLE_IMAGE_ID_BIT | NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let inner = Arc::new(Inner {
            id,
            len: bytes.len(),
            encoded: encoded.into(),
            cors_status,
            decoded: Mutex::new(Some(bytes.clone())),
            last_used: AtomicU64::new(now_ms()),
        });
        DECODED_TOTAL.fetch_add(inner.len, Ordering::Relaxed);
        BY_ID.write().unwrap().insert(id, Arc::downgrade(&inner));
        ALL.lock().unwrap().push(Arc::downgrade(&inner));
        *self = ImageBytes::Discardable(inner);
        discard_if_over_budget();
    }
}

/// WebRender's lock of a discardable image: its pixels, decoded again if needed. `None` if the
/// image is gone (WebRender then uploads blank pixels until the display list stops using it).
pub fn lock_external_image(id: u64) -> Option<Arc<Vec<u8>>> {
    let inner = BY_ID.read().unwrap().get(&id).and_then(Weak::upgrade)?;
    Some(inner.get())
}

/// Total decoded bytes held by discardable images, for memory reports.
pub fn discardable_decoded_bytes() -> usize {
    DECODED_TOTAL.load(Ordering::Relaxed)
}

impl MallocSizeOf for ImageBytes {
    fn size_of(&self, ops: &mut MallocSizeOfOps) -> usize {
        self.conditional_size_of(ops)
    }
}

impl MallocConditionalSizeOf for ImageBytes {
    fn conditional_size_of(&self, ops: &mut MallocSizeOfOps) -> usize {
        match self {
            ImageBytes::Plain(bytes) => bytes.conditional_size_of(ops),
            ImageBytes::Discardable(inner) => {
                if ops.have_seen_ptr(Arc::as_ptr(inner)) {
                    return 0;
                }
                let decoded = if inner.decoded.lock().unwrap().is_some() {
                    inner.len
                } else {
                    0
                };
                decoded + inner.encoded.len()
            },
        }
    }
}
