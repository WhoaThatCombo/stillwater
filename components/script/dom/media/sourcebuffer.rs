/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::{Cell, Ref};
use std::collections::{BTreeMap, HashMap};

use dom_struct::dom_struct;
use js::context::JSContext;
use script_bindings::cell::DomRefCell;
use script_bindings::reflector::reflect_dom_object;
use stylo_atoms::Atom;

use crate::dom::bindings::buffer_source::get_buffer_source_copy;
use crate::dom::bindings::codegen::Bindings::SourceBufferBinding::{
    AppendMode, SourceBufferMethods,
};
use crate::dom::bindings::codegen::UnionTypes::ArrayBufferViewOrArrayBuffer;
use crate::dom::bindings::error::{Error, ErrorResult};
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::num::Finite;
use crate::dom::bindings::refcounted::Trusted;
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::{Dom, DomRoot};
use crate::dom::bindings::str::DOMString;
use crate::dom::eventtarget::EventTarget;
use crate::dom::fmp4::{
    InitSegment, TrackInfo, TrackKind, parse_fragment, parse_init_segment, read_box,
};
use crate::dom::mediasource::MediaSource;
use crate::dom::timeranges::{TimeRanges, TimeRangesContainer};
use crate::dom::webm::{WebmOutput, WebmParser};
use crate::dom::window::Window;

/// Most media a video `SourceBuffer` may hold before appends fail with `QuotaExceededError`
/// (after evicting what's already been played); sites then remove old data themselves. The same
/// limits as Chrome, which sites' buffering strategies are tuned for.
const MAX_VIDEO_BYTES: usize = 150 * 1024 * 1024;
/// The same for an audio `SourceBuffer`.
const MAX_AUDIO_BYTES: usize = 12 * 1024 * 1024;

/// Media that ended this long before the playhead is dropped whenever more is appended, so a
/// long video doesn't keep everything it played in memory. (Sites usually trim it themselves.)
const BACK_BUFFER_SECONDS: f64 = 30.0;

/// When over quota, evict played media up to this long before the playhead.
const MIN_BACK_BUFFER_SECONDS: f64 = 3.0;

/// Two times closer than this are treated as the same (fragment boundaries, buffered ranges).
const EPSILON: f64 = 0.02;

/// One media segment (`moof` + `mdat`), with its place on the media timeline.
pub(crate) struct Fragment {
    pub(crate) start: f64,
    pub(crate) end: f64,
    /// The original bytes.
    pub(crate) bytes: Vec<u8>,
    /// Which initialization segment it belongs to (changes when a site switches quality).
    pub(crate) generation: u32,
    /// Whether it starts with a keyframe, so playback can (re)start at it.
    pub(crate) keyframe: bool,
}

/// What a `SourceBuffer` holds: its initialization segments and parsed media segments.
#[derive(Default)]
pub(crate) struct TrackBuffer {
    /// Appended bytes that don't form a complete box yet.
    pending: Vec<u8>,
    ftyp: Option<Vec<u8>>,
    moof: Option<Vec<u8>>,
    /// Initialization segments by generation (only recent ones are kept).
    inits: HashMap<u32, InitSegment>,
    pub(crate) generation: u32,
    /// Fragments by start time, in microseconds.
    fragments: BTreeMap<u64, Fragment>,
    bytes: usize,
    /// Set while the appended byte stream is WebM, which is remuxed to fragmented MP4.
    webm: Option<Box<WebmParser>>,
}

impl TrackBuffer {
    /// The latest initialization segment's (first) track.
    pub(crate) fn track(&self) -> Option<&TrackInfo> {
        self.inits.get(&self.generation)?.tracks.first()
    }

    pub(crate) fn init(&self, generation: u32) -> Option<(&InitSegment, &TrackInfo)> {
        let init = self.inits.get(&generation)?;
        Some((init, init.tracks.first()?))
    }

    /// Parse newly appended bytes. `Err` means the data isn't valid fragmented MP4.
    fn append(&mut self, data: &[u8], timestamp_offset: f64) -> Result<(), ()> {
        // Which byte stream format this is: sniffed at the start of an initialization segment
        // (sites may switch with changeType()).
        if self.pending.is_empty() && self.moof.is_none() {
            if data.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]) {
                if self.webm.is_none() {
                    self.webm = Some(Box::default());
                }
            } else if matches!(data.get(4..8), Some(b"ftyp" | b"styp" | b"moov")) {
                self.webm = None;
            }
        }
        if let Some(webm) = self.webm.as_mut() {
            for output in webm.append(data)? {
                match output {
                    WebmOutput::Init(init) => self.add_init(init)?,
                    WebmOutput::Fragment(fragment) => {
                        self.add_fragment(fragment, timestamp_offset)?
                    },
                }
            }
            return Ok(());
        }
        self.pending.extend_from_slice(data);
        let mut consumed = 0;
        while let Some(found) = read_box(&self.pending, consumed)? {
            consumed = found.range.end;
            let bytes = self.pending[found.range.clone()].to_vec();
            match &found.kind {
                b"ftyp" => self.ftyp = Some(bytes),
                b"moov" => {
                    let mut init = self.ftyp.take().unwrap_or_default();
                    init.extend_from_slice(&bytes);
                    self.add_init(init)?;
                },
                b"moof" => self.moof = Some(bytes),
                b"mdat" => {
                    if let Some(mut fragment) = self.moof.take() {
                        fragment.extend_from_slice(&bytes);
                        self.add_fragment(fragment, timestamp_offset)?;
                    }
                },
                // styp, sidx, emsg, prft, free, …: not needed for playback.
                _ => {},
            }
        }
        self.pending.drain(..consumed);
        Ok(())
    }

    /// A new initialization segment (`ftyp` + `moov`); a different one starts a new generation.
    fn add_init(&mut self, init: Vec<u8>) -> Result<(), ()> {
        let parsed = parse_init_segment(&init).ok_or(())?;
        let same = self
            .inits
            .get(&self.generation)
            .is_some_and(|current| current.raw == parsed.raw);
        if !same {
            self.generation += 1;
            self.inits.insert(self.generation, parsed);
            self.forget_unused_inits();
        }
        Ok(())
    }

    /// Drop initialization segments that no buffered fragment uses (except the current one).
    fn forget_unused_inits(&mut self) {
        let generation = self.generation;
        let used: Vec<u32> = self
            .fragments
            .values()
            .map(|fragment| fragment.generation)
            .collect();
        self.inits
            .retain(|other, _| *other == generation || used.contains(other));
    }

    fn add_fragment(&mut self, bytes: Vec<u8>, timestamp_offset: f64) -> Result<(), ()> {
        let track = self.track().ok_or(())?;
        let timescale = track.timescale.max(1) as f64;
        let moof = read_box(&bytes, 0)?.ok_or(())?;
        let info = parse_fragment(
            &bytes,
            &moof,
            track.default_sample_duration,
            track.default_sample_flags,
        )
        .ok_or(())?;
        let start = info.decode_time as f64 / timescale + timestamp_offset;
        let end = start + info.duration as f64 / timescale;

        // Newer media replaces whatever covered the same time (e.g. a different quality).
        let overlapping: Vec<u64> = self
            .fragments
            .iter()
            .filter(|(_, other)| other.start < end - EPSILON && other.end > start + EPSILON)
            .map(|(key, _)| *key)
            .collect();
        for key in overlapping {
            if let Some(old) = self.fragments.remove(&key) {
                self.bytes -= old.bytes.len();
            }
        }
        self.bytes += bytes.len();
        self.fragments.insert(
            (start.max(0.0) * 1_000_000.0) as u64,
            Fragment {
                start,
                end,
                bytes,
                generation: self.generation,
                keyframe: info.starts_with_sync,
            },
        );
        Ok(())
    }

    /// The first fragment that still has media after `time`.
    pub(crate) fn fragment_after(&self, time: f64) -> Option<&Fragment> {
        self.fragments
            .values()
            .find(|fragment| fragment.end > time + EPSILON)
    }

    /// The variant (initialization segment generation) playing at `time`: that of the fragment
    /// starting last at or before it, else of the first one after it.
    pub(crate) fn generation_at(&self, time: f64) -> Option<u32> {
        let mut generation = None;
        for fragment in self.fragments.values() {
            if fragment.start > time + EPSILON {
                return generation.or(Some(fragment.generation));
            }
            generation = Some(fragment.generation);
        }
        generation
    }

    /// Where playback restarted at `time` must begin: the start of the last fragment at or
    /// before `time` that starts with a keyframe (or the first fragment after `time`).
    pub(crate) fn fragment_start_at(&self, time: f64) -> Option<f64> {
        let mut start = None;
        for fragment in self.fragments.values() {
            if fragment.start > time + EPSILON {
                break;
            }
            if fragment.keyframe {
                start = Some(fragment.start);
            }
        }
        start
            .or_else(|| self.fragment_after(time).map(|fragment| fragment.start))
            .map(|start| start.min(time))
    }

    /// Buffered time ranges (fragments merged when they touch).
    pub(crate) fn ranges(&self) -> Vec<(f64, f64)> {
        let mut ranges: Vec<(f64, f64)> = vec![];
        for fragment in self.fragments.values() {
            match ranges.last_mut() {
                Some(last) if fragment.start <= last.1 + EPSILON => {
                    last.1 = last.1.max(fragment.end);
                },
                _ => ranges.push((fragment.start, fragment.end)),
            }
        }
        ranges
    }

    pub(crate) fn highest_end(&self) -> f64 {
        self.fragments
            .values()
            .map(|fragment| fragment.end)
            .fold(0.0, f64::max)
    }

    /// Drop fragments lying entirely within `[start, end]`.
    fn remove(&mut self, start: f64, end: f64) {
        let doomed: Vec<u64> = self
            .fragments
            .iter()
            .filter(|(_, fragment)| {
                fragment.start >= start - EPSILON && fragment.end <= end + EPSILON
            })
            .map(|(key, _)| *key)
            .collect();
        for key in doomed {
            if let Some(old) = self.fragments.remove(&key) {
                self.bytes -= old.bytes.len();
            }
        }
    }

    /// Forget a half-appended segment (`abort()`, `changeType()`).
    fn reset_parser(&mut self) {
        self.pending.clear();
        self.moof = None;
        self.ftyp = None;
        if self.webm.is_some() {
            self.webm = Some(Box::default());
        }
    }
}

/// <https://w3c.github.io/media-source/#sourcebuffer>
#[dom_struct]
pub(crate) struct SourceBuffer {
    eventtarget: EventTarget,
    parent: Dom<MediaSource>,
    updating: Cell<bool>,
    sequence_mode: Cell<bool>,
    timestamp_offset: Cell<f64>,
    append_window_start: Cell<f64>,
    append_window_end: Cell<f64>,
    removed: Cell<bool>,
    #[no_trace]
    #[ignore_malloc_size_of = "Media data, accounted for by the quota"]
    track: DomRefCell<TrackBuffer>,
}

impl SourceBuffer {
    pub(crate) fn new(
        cx: &mut JSContext,
        window: &Window,
        parent: &MediaSource,
    ) -> DomRoot<SourceBuffer> {
        reflect_dom_object(
            cx,
            Box::new(SourceBuffer {
                eventtarget: EventTarget::new_inherited(),
                parent: Dom::from_ref(parent),
                updating: Cell::new(false),
                sequence_mode: Cell::new(false),
                timestamp_offset: Cell::new(0.0),
                append_window_start: Cell::new(0.0),
                append_window_end: Cell::new(f64::INFINITY),
                removed: Cell::new(false),
                track: DomRefCell::new(TrackBuffer::default()),
            }),
            window,
        )
    }

    pub(crate) fn track(&self) -> Ref<'_, TrackBuffer> {
        self.track.borrow()
    }

    pub(crate) fn is_updating(&self) -> bool {
        self.updating.get()
    }

    pub(crate) fn set_removed(&self) {
        self.removed.set(true);
        self.track.borrow_mut().reset_parser();
    }

    fn check_usable(&self) -> ErrorResult {
        if self.removed.get() {
            return Err(Error::InvalidState(Some("SourceBuffer was removed".into())));
        }
        if self.updating.get() {
            return Err(Error::InvalidState(Some("SourceBuffer is updating".into())));
        }
        Ok(())
    }

    /// Mark the buffer updating and fire `updatestart`; `finish` runs as a task afterwards and
    /// fires `update` + `updateend` (the spec's asynchronous half of append/remove).
    fn start_update(&self, finish: impl FnOnce(&SourceBuffer) -> bool + Send + 'static) {
        self.updating.set(true);
        self.queue_event("updatestart");
        let this = Trusted::new(self);
        self.global()
            .task_manager()
            .media_element_task_source()
            .queue(task!(source_buffer_update: move |cx| {
                let this = this.root();
                if !this.updating.get() {
                    // abort() already finished this update.
                    return;
                }
                let ok = finish(&this);
                this.updating.set(false);
                let target = this.upcast::<EventTarget>();
                if ok {
                    target.fire_event(cx, Atom::from("update"));
                } else {
                    target.fire_event(cx, Atom::from("error"));
                }
                target.fire_event(cx, Atom::from("updateend"));
                this.parent.data_changed();
            }));
    }

    fn queue_event(&self, name: &'static str) {
        let this = Trusted::new(self);
        self.global()
            .task_manager()
            .media_element_task_source()
            .queue(task!(source_buffer_event: move |cx| {
                this.root().upcast::<EventTarget>().fire_event(cx, Atom::from(name));
            }));
    }
}

impl SourceBufferMethods<crate::DomTypeHolder> for SourceBuffer {
    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-mode>
    fn Mode(&self) -> AppendMode {
        if self.sequence_mode.get() {
            AppendMode::Sequence
        } else {
            AppendMode::Segments
        }
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-mode>
    fn SetMode(&self, mode: AppendMode) -> ErrorResult {
        self.check_usable()?;
        self.sequence_mode.set(mode == AppendMode::Sequence);
        Ok(())
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-updating>
    fn Updating(&self) -> bool {
        self.updating.get()
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-buffered>
    fn Buffered(&self, cx: &mut JSContext) -> DomRoot<TimeRanges> {
        let mut ranges = TimeRangesContainer::default();
        if !self.removed.get() {
            for (start, end) in self.track.borrow().ranges() {
                let _ = ranges.add(start, end);
            }
        }
        TimeRanges::new(cx, self.global().as_window(), ranges)
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-timestampoffset>
    fn TimestampOffset(&self) -> Finite<f64> {
        Finite::wrap(self.timestamp_offset.get())
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-timestampoffset>
    fn SetTimestampOffset(&self, offset: Finite<f64>) -> ErrorResult {
        self.check_usable()?;
        self.timestamp_offset.set(*offset);
        Ok(())
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-appendwindowstart>
    fn AppendWindowStart(&self) -> Finite<f64> {
        Finite::wrap(self.append_window_start.get())
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-appendwindowstart>
    fn SetAppendWindowStart(&self, start: Finite<f64>) -> ErrorResult {
        self.check_usable()?;
        if *start < 0.0 || *start >= self.append_window_end.get() {
            return Err(Error::Type(c"appendWindowStart out of range".to_owned()));
        }
        self.append_window_start.set(*start);
        Ok(())
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-appendwindowend>
    fn AppendWindowEnd(&self) -> f64 {
        self.append_window_end.get()
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-appendwindowend>
    fn SetAppendWindowEnd(&self, end: f64) -> ErrorResult {
        self.check_usable()?;
        if end.is_nan() || end <= self.append_window_start.get() {
            return Err(Error::Type(c"appendWindowEnd out of range".to_owned()));
        }
        self.append_window_end.set(end);
        Ok(())
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-appendbuffer>
    fn AppendBuffer(&self, data: ArrayBufferViewOrArrayBuffer) -> ErrorResult {
        self.check_usable()?;
        let bytes = match &data {
            ArrayBufferViewOrArrayBuffer::ArrayBufferView(view) => get_buffer_source_copy(view.into()),
            ArrayBufferViewOrArrayBuffer::ArrayBuffer(buffer) => get_buffer_source_copy(buffer.into()),
        };
        self.parent.reopen_if_ended();

        // Coded frame eviction: make room by dropping media well behind the playhead.
        {
            let mut track = self.track.borrow_mut();
            if track.bytes + bytes.len() > MAX_BUFFERED_BYTES {
                let played = self.parent.current_time() - 10.0;
                if played > 0.0 {
                    track.remove(0.0, played);
                }
                if track.bytes + bytes.len() > MAX_BUFFERED_BYTES {
                    return Err(Error::QuotaExceeded {
                        quota: None,
                        requested: None,
                    });
                }
            }
        }

        let offset = self.timestamp_offset.get();
        self.start_update(move |this| this.track.borrow_mut().append(&bytes, offset).is_ok());
        Ok(())
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-abort>
    fn Abort(&self) -> ErrorResult {
        if self.removed.get() {
            return Err(Error::InvalidState(None));
        }
        self.track.borrow_mut().reset_parser();
        if self.updating.get() {
            self.updating.set(false);
            self.queue_event("abort");
            self.queue_event("updateend");
        }
        self.append_window_start.set(0.0);
        self.append_window_end.set(f64::INFINITY);
        Ok(())
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-changetype>
    fn ChangeType(&self, type_: DOMString) -> ErrorResult {
        if type_.is_empty() {
            return Err(Error::Type(c"Empty type".to_owned()));
        }
        self.check_usable()?;
        if !MediaSource::type_supported(&type_.to_string()) {
            return Err(Error::NotSupported(None));
        }
        self.track.borrow_mut().reset_parser();
        Ok(())
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-remove>
    fn Remove(&self, start: Finite<f64>, end: f64) -> ErrorResult {
        self.check_usable()?;
        if *start < 0.0 || end.is_nan() || end <= *start {
            return Err(Error::Type(c"Invalid range".to_owned()));
        }
        let start = *start;
        self.parent.reopen_if_ended();
        self.start_update(move |this| {
            this.track.borrow_mut().remove(start, end);
            true
        });
        Ok(())
    }

    // <https://w3c.github.io/media-source/#dom-sourcebuffer-onupdatestart>
    event_handler!(updatestart, GetOnupdatestart, SetOnupdatestart);

    // <https://w3c.github.io/media-source/#dom-sourcebuffer-onupdate>
    event_handler!(update, GetOnupdate, SetOnupdate);

    // <https://w3c.github.io/media-source/#dom-sourcebuffer-onupdateend>
    event_handler!(updateend, GetOnupdateend, SetOnupdateend);

    // <https://w3c.github.io/media-source/#dom-sourcebuffer-onerror>
    event_handler!(error, GetOnerror, SetOnerror);

    // <https://w3c.github.io/media-source/#dom-sourcebuffer-onabort>
    event_handler!(abort, GetOnabort, SetOnabort);
}
