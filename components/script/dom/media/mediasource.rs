/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Media Source Extensions: <https://w3c.github.io/media-source/>
//!
//! Supports fragmented MP4 (H.264 video, AAC audio), which is what YouTube and most streaming
//! sites use. The media element's player is fed one merged byte stream built from all the
//! `SourceBuffer`s (see `fmp4.rs`); seeking restarts that stream from the nearest fragment.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};

use dom_struct::dom_struct;
use js::context::JSContext;
use js::rust::HandleObject;
use script_bindings::cell::DomRefCell;
use script_bindings::reflector::reflect_dom_object_with_proto;
use stylo_atoms::Atom;
use uuid::Uuid;

use crate::dom::bindings::codegen::Bindings::MediaSourceBinding::{
    EndOfStreamError, MediaSourceMethods, MediaSourceReadyState,
};
use crate::dom::bindings::error::{Error, ErrorResult, Fallible};
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::num::Finite;
use crate::dom::bindings::refcounted::Trusted;
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::{Dom, DomRoot, MutNullableDom};
use crate::dom::bindings::str::DOMString;
use crate::dom::eventtarget::EventTarget;
use crate::dom::fmp4::{
    InitSegment, TrackInfo, combined_init_segment, set_chunk_sequence, split_fragment,
};
use crate::dom::globalscope::GlobalScope;
use crate::dom::html::htmlmediaelement::HTMLMediaElement;
use crate::dom::sourcebuffer::SourceBuffer;
use crate::dom::sourcebufferlist::SourceBufferList;
use crate::dom::timeranges::TimeRangesContainer;
use crate::dom::url::URL;
use crate::dom::window::Window;

thread_local! {
    /// Object URLs made by `URL.createObjectURL(mediaSource)`, until revoked.
    static OBJECT_URLS: RefCell<HashMap<String, Trusted<MediaSource>>> = RefCell::new(HashMap::new());
}

/// What the media element's player should get next.
pub(crate) enum MseChunk {
    Data(Vec<u8>),
    EndOfStream,
    /// A stream switched variant (e.g. quality) at this time: playback must restart there
    /// with a fresh pipeline, since the old one was set up for the previous variant.
    Restart(f64),
}

/// Progress of the merged stream fed to the current player.
#[derive(Default)]
struct Feed {
    started: bool,
    /// Media time that the player's time zero corresponds to.
    base: f64,
    /// The initialization segment generation each buffer's track was started with.
    generations: Vec<u32>,
    /// Per buffer: media time up to which fragments have been fed.
    next: Vec<f64>,
    sequence: u32,
    end_of_stream_sent: bool,
    /// Per buffer: pieces cut from its current fragment, waiting to be fed (media start time).
    queues: Vec<VecDeque<(f64, Vec<u8>)>>,
    /// Where playback (re)started: each buffer starts with the variant playing there.
    start: f64,
    /// A variant with another codec begins at this time: the stream ends there, and playback
    /// restarts with a new pipeline once it gets there (see [`MediaSource::take_pending_switch`]).
    switch_at: Option<f64>,
}

/// Two times closer than this are the same (matches `SourceBuffer`'s fragment tolerance).
const EPSILON: f64 = 0.02;

/// The most video pixels per second `isTypeSupported` admits (when a type gives its size):
/// software decoding plus conversion of the frames must keep up in real time.
const MAX_PIXELS_PER_SECOND: f64 = 3840.0 * 2160.0 * 60.0;

/// Whether one `codecs` entry can be played from an MP4 or WebM `SourceBuffer`.
fn codec_supported(codec: &str, webm: bool, audio_only: bool) -> bool {
    // VP9 in "vp09.PP.LL.DD[...]" form: profiles 0 and 1 at 8 bits (not 10-bit / HDR).
    let vp09 = || {
        let mut fields = codec.split('.').skip(1);
        let profile = fields.next().unwrap_or("00");
        let _level = fields.next();
        let depth = fields.next().unwrap_or("08");
        matches!(profile, "00" | "01") && depth == "08"
    };
    let video = match codec {
        "vp8" | "vp8.0" | "vp9" | "vp9.0" if webm => true,
        _ if codec.starts_with("vp09.") => vp09(),
        _ if codec.starts_with("avc1.") || codec.starts_with("avc3.") => !webm,
        _ => false,
    };
    if video {
        return !audio_only;
    }
    match codec {
        "opus" => true,
        "mp4a.40.2" | "mp4a.40.5" | "mp4a.40.29" | "mp4a.40" | "mp4a.67" => !webm,
        // MP3 and FLAC in MP4, like Chrome.
        "mp3" | "mp4a.69" | "mp4a.6b" | "mp4a.40.34" | "flac" => !webm,
        _ => false,
    }
}

/// Length of the pieces fragments are cut into, so audio and video interleave finely.
const PIECE_SECONDS: f64 = 0.5;

#[derive(Clone, Copy, JSTraceable, MallocSizeOf, PartialEq)]
enum State {
    Closed,
    Open,
    Ended,
}

/// <https://w3c.github.io/media-source/#mediasource>
#[dom_struct]
pub(crate) struct MediaSource {
    eventtarget: EventTarget,
    source_buffers: Dom<SourceBufferList>,
    state: Cell<State>,
    duration: Cell<f64>,
    element: MutNullableDom<HTMLMediaElement>,
    #[no_trace]
    #[ignore_malloc_size_of = "Plain bookkeeping"]
    feed: DomRefCell<Feed>,
}

impl MediaSource {
    /// Whether `type` (a MIME type with optional `codecs`) can be played through MSE.
    ///
    /// Fragmented MP4 with H.264, VP9 (8-bit), AAC, MP3, FLAC or Opus, and WebM with VP8, VP9
    /// (8-bit) or Opus, in any combination across buffers. WebM is remuxed to MP4 (see `webm.rs`).
    /// Videos too big to decode in software in real time are refused (see
    /// [`MAX_PIXELS_PER_SECOND`]), so adaptive players pick a variant that plays smoothly.
    pub(crate) fn type_supported(type_: &str) -> bool {
        let mut parts = type_.split(';');
        let container = parts.next().unwrap_or_default().trim().to_ascii_lowercase();
        let (webm, audio_only) = match container.as_str() {
            "video/mp4" => (false, false),
            "audio/mp4" => (false, true),
            "video/webm" => (true, false),
            "audio/webm" => (true, true),
            _ => return false,
        };
        let mut width = 0f64;
        let mut height = 0f64;
        let mut framerate = 0f64;
        for parameter in parts {
            let Some((name, value)) = parameter.split_once('=') else {
                continue;
            };
            let name = name.trim().to_ascii_lowercase();
            let value = value.trim().trim_matches('"').to_ascii_lowercase();
            match name.as_str() {
                "codecs" => {
                    let supported = value
                        .split(',')
                        .map(str::trim)
                        .all(|codec| codec_supported(codec, webm, audio_only));
                    if !supported {
                        return false;
                    }
                },
                "width" => width = value.parse().unwrap_or(0.0),
                "height" => height = value.parse().unwrap_or(0.0),
                "framerate" => framerate = value.parse().unwrap_or(0.0),
                // HDR transfer functions and encrypted media aren't supported.
                "eotf" if value != "bt709" => return false,
                "cryptoblockformat" => return false,
                _ => {},
            }
        }
        let framerate = if framerate > 0.0 { framerate } else { 30.0 };
        width * height * framerate <= MAX_PIXELS_PER_SECOND
    }

    /// The `MediaSource` behind an object URL, if `url` is one.
    pub(crate) fn from_object_url(url: &str) -> Option<DomRoot<MediaSource>> {
        OBJECT_URLS.with(|urls| urls.borrow().get(url).and_then(|(source, _)| source.root()))
    }

    /// `URL.createObjectURL(mediaSource)`.
    pub(crate) fn create_object_url(&self, global: &GlobalScope) -> DOMString {
        let url = URL::unicode_serialization_blob_url(global.origin().immutable(), &Uuid::new_v4());
        OBJECT_URLS.with(|urls| {
            let mut urls = urls.borrow_mut();
            urls.retain(|_, (source, _)| source.is_alive());
            urls.insert(url.clone(), (WeakRef::new(self), Some(Trusted::new(self))));
        });
        DOMString::from(url)
    }

    /// `URL.revokeObjectURL(url)`; returns whether it was a `MediaSource` URL.
    pub(crate) fn revoke_object_url(url: &str) -> bool {
        OBJECT_URLS.with(|urls| urls.borrow_mut().remove(url).is_some())
    }

    fn queue_event(&self, name: &'static str) {
        let this = Trusted::new(self);
        self.global()
            .task_manager()
            .media_element_task_source()
            .queue(task!(media_source_event: move |cx| {
                this.root().upcast::<EventTarget>().fire_event(cx, Atom::from(name));
            }));
    }

    /// A media element started using this source (its `src` is our object URL).
    pub(crate) fn attach(&self, element: &HTMLMediaElement) {
        self.element.set(Some(element));
        *self.feed.borrow_mut() = Feed::default();
        self.state.set(State::Open);
        self.queue_event("sourceopen");
    }

    /// The media element stopped using this source.
    pub(crate) fn detach(&self) {
        self.element.set(None);
        self.state.set(State::Closed);
        self.duration.set(f64::NAN);
        for buffer in self.source_buffers.buffers() {
            buffer.set_removed();
        }
        self.source_buffers.clear();
        self.queue_event("sourceclose");
    }

    /// Appends or removals finished: give the player whatever it can use now.
    pub(crate) fn data_changed(&self) {
        if let Some(element) = self.element.get() {
            element.mse_feed();
        }
    }

    /// Appending to an ended source reopens it.
    pub(crate) fn reopen_if_ended(&self) {
        if self.state.get() == State::Ended {
            self.state.set(State::Open);
            self.feed.borrow_mut().end_of_stream_sent = false;
            self.queue_event("sourceopen");
        }
    }

    pub(crate) fn current_time(&self) -> f64 {
        self.element
            .get()
            .map(|element| element.mse_current_time())
            .unwrap_or(0.0)
    }

    /// `duration` as the media element reports it.
    pub(crate) fn element_duration(&self) -> f64 {
        self.duration.get()
    }

    /// The media element's `buffered`: time buffered in every `SourceBuffer`.
    pub(crate) fn buffered(&self) -> TimeRangesContainer {
        let buffers = self.source_buffers.buffers();
        let mut intersection: Option<Vec<(f64, f64)>> = None;
        for buffer in &buffers {
            let ranges = buffer.track().ranges();
            intersection = Some(match intersection {
                None => ranges,
                Some(previous) => {
                    let mut both = vec![];
                    for (a_start, a_end) in &previous {
                        for (b_start, b_end) in &ranges {
                            let start = a_start.max(*b_start);
                            let end = a_end.min(*b_end);
                            if end > start {
                                both.push((start, end));
                            }
                        }
                    }
                    both
                },
            });
        }
        let mut container = TimeRangesContainer::default();
        for (start, end) in intersection.unwrap_or_default() {
            let _ = container.add(start, end);
        }
        container
    }

    /// Restart the merged stream so the player's time zero is at (or just before) `time`.
    /// Returns the media time the new stream starts at.
    pub(crate) fn restart_feed(&self, time: f64) -> f64 {
        let buffers = self.source_buffers.buffers();
        let base = buffers
            .iter()
            .filter_map(|buffer| buffer.track().fragment_start_at(time))
            .fold(time, f64::min)
            .max(0.0);
        *self.feed.borrow_mut() = Feed {
            base,
            ..Feed::default()
        };
        base
    }

    /// The next piece of the merged stream for the player, if one is ready.
    pub(crate) fn next_chunk(&self) -> Option<MseChunk> {
        let buffers = self.source_buffers.buffers();
        if buffers.is_empty() {
            return None;
        }
        let tracks: Vec<_> = buffers.iter().map(|buffer| buffer.track()).collect();
        let mut feed = self.feed.borrow_mut();

        if !feed.started {
            // Start with the variant of each buffer's first fragment to play.
            let mut generations = vec![];
            for track in &tracks {
                let generation = track
                    .fragment_after(feed.base)
                    .map(|fragment| fragment.generation)
                    .unwrap_or(track.generation);
                track.init(generation)?;
                generations.push(generation);
            }
            let inits: Vec<(&InitSegment, &TrackInfo)> = tracks
                .iter()
                .zip(&generations)
                .filter_map(|(track, generation)| track.init(*generation))
                .collect();
            let init = combined_init_segment(&inits);
            let base = feed.base;
            feed.started = true;
            feed.next = vec![base; tracks.len()];
            feed.queues = vec![VecDeque::new(); tracks.len()];
            feed.generations = generations;
            return Some(MseChunk::Data(init));
        }
        if feed.next.len() != tracks.len() {
            // A buffer was added or removed mid-stream.
            let position = self.current_time();
            return Some(MseChunk::Restart(position));
        }

        // Cut each buffer's next fragment into short pieces (see `split_fragment`).
        for (index, track) in tracks.iter().enumerate() {
            if !feed.queues[index].is_empty() {
                continue;
            }
            let Some(fragment) = track.fragment_after(feed.next[index]) else {
                continue;
            };
            if fragment.generation != feed.generations[index] {
                return Some(MseChunk::Restart(fragment.start.max(feed.next[index])));
            }
            let end = fragment.end;
            let Some((_, info)) = track.init(fragment.generation) else {
                feed.next[index] = end;
                continue;
            };
            let timescale = info.timescale.max(1) as f64;
            let shift = (feed.base * timescale).round().max(0.0) as u64;
            let pieces = split_fragment(
                &fragment.bytes,
                info,
                index as u32 + 1,
                shift,
                (timescale * PIECE_SECONDS) as u64,
            )
            .unwrap_or_default();
            let base = feed.base;
            for piece in pieces {
                let start = piece.decode_time as f64 / timescale + base;
                feed.queues[index].push_back((start, piece.bytes));
            }
            feed.next[index] = end;
        }

        // Feed pieces in time order across buffers, never running ahead of a buffer that has
        // nothing yet (unless the stream has ended).
        let ended = self.state.get() == State::Ended;
        let mut best: Option<(usize, f64)> = None;
        for index in 0..tracks.len() {
            match feed.queues[index].front() {
                Some((start, _)) => {
                    if best.is_none_or(|(_, earliest)| *start < earliest) {
                        best = Some((index, *start));
                    }
                },
                None if !ended => return None,
                None => {},
            }
        }
        let Some((index, _)) = best else {
            if ended && !feed.end_of_stream_sent {
                feed.end_of_stream_sent = true;
                return Some(MseChunk::EndOfStream);
            }
            return None;
        };
        let (_, mut bytes) = feed.queues[index].pop_front()?;
        set_chunk_sequence(&mut bytes, feed.sequence);
        feed.sequence = feed.sequence.wrapping_add(1);
        Some(MseChunk::Data(bytes))
    }

    fn any_updating(&self) -> bool {
        self.source_buffers
            .buffers()
            .iter()
            .any(|buffer| buffer.is_updating())
    }
}

impl MediaSourceMethods<crate::DomTypeHolder> for MediaSource {
    /// <https://w3c.github.io/media-source/#dom-mediasource-constructor>
    fn Constructor(
        cx: &mut JSContext,
        window: &Window,
        proto: Option<HandleObject>,
    ) -> Fallible<DomRoot<MediaSource>> {
        let source_buffers = SourceBufferList::new(cx, window);
        Ok(reflect_dom_object_with_proto(
            cx,
            Box::new(MediaSource {
                eventtarget: EventTarget::new_inherited(),
                source_buffers: Dom::from_ref(&*source_buffers),
                state: Cell::new(State::Closed),
                duration: Cell::new(f64::NAN),
                element: MutNullableDom::new(None),
                feed: DomRefCell::new(Feed::default()),
            }),
            window,
            proto,
        ))
    }

    /// <https://w3c.github.io/media-source/#dom-mediasource-sourcebuffers>
    fn SourceBuffers(&self) -> DomRoot<SourceBufferList> {
        DomRoot::from_ref(&*self.source_buffers)
    }

    /// <https://w3c.github.io/media-source/#dom-mediasource-activesourcebuffers>
    fn ActiveSourceBuffers(&self) -> DomRoot<SourceBufferList> {
        DomRoot::from_ref(&*self.source_buffers)
    }

    /// <https://w3c.github.io/media-source/#dom-mediasource-readystate>
    fn ReadyState(&self) -> MediaSourceReadyState {
        match self.state.get() {
            State::Closed => MediaSourceReadyState::Closed,
            State::Open => MediaSourceReadyState::Open,
            State::Ended => MediaSourceReadyState::Ended,
        }
    }

    /// <https://w3c.github.io/media-source/#dom-mediasource-duration>
    fn Duration(&self) -> f64 {
        if self.state.get() == State::Closed {
            return f64::NAN;
        }
        self.duration.get()
    }

    /// <https://w3c.github.io/media-source/#dom-mediasource-duration>
    fn SetDuration(&self, duration: f64) -> ErrorResult {
        if duration.is_nan() || duration < 0.0 {
            return Err(Error::Type(c"Invalid duration".to_owned()));
        }
        if self.state.get() != State::Open || self.any_updating() {
            return Err(Error::InvalidState(None));
        }
        if self.duration.get() == duration {
            return Ok(());
        }
        self.duration.set(duration);
        if let Some(element) = self.element.get() {
            element.mse_duration_changed();
        }
        Ok(())
    }

    /// <https://w3c.github.io/media-source/#dom-mediasource-addsourcebuffer>
    fn AddSourceBuffer(
        &self,
        cx: &mut JSContext,
        type_: DOMString,
    ) -> Fallible<DomRoot<SourceBuffer>> {
        if type_.is_empty() {
            return Err(Error::Type(c"Empty type".to_owned()));
        }
        if !Self::type_supported(&type_.to_string()) {
            return Err(Error::NotSupported(None));
        }
        if self.state.get() != State::Open {
            return Err(Error::InvalidState(None));
        }
        // One video and one audio buffer is all the merged stream handles.
        if self.source_buffers.buffers().len() >= 2 {
            return Err(Error::QuotaExceeded {
                quota: None,
                requested: None,
            });
        }
        let buffer = SourceBuffer::new(cx, self.global().as_window(), self);
        self.source_buffers.push(&buffer);
        Ok(buffer)
    }

    /// <https://w3c.github.io/media-source/#dom-mediasource-removesourcebuffer>
    fn RemoveSourceBuffer(&self, buffer: &SourceBuffer) -> ErrorResult {
        if !self.source_buffers.contains(buffer) {
            return Err(Error::NotFound(None));
        }
        buffer.set_removed();
        self.source_buffers.remove(buffer);
        self.data_changed();
        Ok(())
    }

    /// <https://w3c.github.io/media-source/#dom-mediasource-endofstream>
    fn EndOfStream(&self, error: Option<EndOfStreamError>) -> ErrorResult {
        if self.state.get() != State::Open || self.any_updating() {
            return Err(Error::InvalidState(None));
        }
        self.state.set(State::Ended);
        self.queue_event("sourceended");
        if error.is_none() {
            // The duration becomes the end of the buffered media.
            let end = self
                .source_buffers
                .buffers()
                .iter()
                .map(|buffer| buffer.track().highest_end())
                .fold(0.0, f64::max);
            if end > 0.0 && (self.duration.get().is_nan() || end != self.duration.get()) {
                self.duration.set(end);
                if let Some(element) = self.element.get() {
                    element.mse_duration_changed();
                }
            }
        }
        self.data_changed();
        Ok(())
    }

    /// <https://w3c.github.io/media-source/#dom-mediasource-setliveseekablerange>
    fn SetLiveSeekableRange(&self, _start: Finite<f64>, _end: Finite<f64>) -> ErrorResult {
        if self.state.get() != State::Open {
            return Err(Error::InvalidState(None));
        }
        Ok(())
    }

    /// <https://w3c.github.io/media-source/#dom-mediasource-clearliveseekablerange>
    fn ClearLiveSeekableRange(&self) -> ErrorResult {
        if self.state.get() != State::Open {
            return Err(Error::InvalidState(None));
        }
        Ok(())
    }

    /// <https://w3c.github.io/media-source/#dom-mediasource-istypesupported>
    fn IsTypeSupported(_window: &Window, type_: DOMString) -> bool {
        Self::type_supported(&type_.to_string())
    }

    // <https://w3c.github.io/media-source/#dom-mediasource-onsourceopen>
    event_handler!(sourceopen, GetOnsourceopen, SetOnsourceopen);

    // <https://w3c.github.io/media-source/#dom-mediasource-onsourceended>
    event_handler!(sourceended, GetOnsourceended, SetOnsourceended);

    // <https://w3c.github.io/media-source/#dom-mediasource-onsourceclose>
    event_handler!(sourceclose, GetOnsourceclose, SetOnsourceclose);
}
