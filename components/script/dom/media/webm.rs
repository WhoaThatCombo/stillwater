/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! WebM byte streams for Media Source Extensions, remuxed into fragmented MP4.
//!
//! A WebM `SourceBuffer` gets an *initialization segment* (EBML header, `Segment` start, `Info`
//! and `Tracks`) and *media segments* (`Cluster`s of `SimpleBlock`s / `BlockGroup`s). Instead of
//! a second playback path, each is rewritten into the ISO-BMFF equivalent that the rest of the
//! MSE code (and GStreamer's `qtdemux`) already handles: a one-track `ftyp` + `moov` (`vp09` /
//! `vp08` with `vpcC`, `Opus` with `dOps`) and one `moof` + `mdat` per cluster. That also lets a
//! WebM buffer play next to an MP4 one (e.g. VP9 video with AAC audio).
//!
//! Frames are copied untouched; only timing is converted (block timestamps become sample
//! durations, since VP8/VP9/Opus never reorder frames).

use crate::dom::fmp4::{NON_SYNC_SAMPLE_FLAGS, SYNC_SAMPLE_FLAGS, Sample, write_box, write_chunk};

const ID_EBML: u32 = 0x1A45_DFA3;
const ID_SEGMENT: u32 = 0x1853_8067;
const ID_INFO: u32 = 0x1549_A966;
const ID_TRACKS: u32 = 0x1654_AE6B;
const ID_CLUSTER: u32 = 0x1F43_B675;
const ID_TIMECODE_SCALE: u32 = 0x2A_D7B1;
const ID_TRACK_ENTRY: u32 = 0xAE;
const ID_TRACK_NUMBER: u32 = 0xD7;
const ID_TRACK_TYPE: u32 = 0x83;
const ID_CODEC_ID: u32 = 0x86;
const ID_CODEC_PRIVATE: u32 = 0x63A2;
const ID_DEFAULT_DURATION: u32 = 0x23_E383;
const ID_CONTENT_ENCODINGS: u32 = 0x6D80;
const ID_VIDEO: u32 = 0xE0;
const ID_PIXEL_WIDTH: u32 = 0xB0;
const ID_PIXEL_HEIGHT: u32 = 0xBA;
const ID_COLOUR: u32 = 0x55B0;
const ID_MATRIX_COEFFICIENTS: u32 = 0x55B1;
const ID_BITS_PER_CHANNEL: u32 = 0x55B2;
const ID_RANGE: u32 = 0x55B9;
const ID_TRANSFER_CHARACTERISTICS: u32 = 0x55BA;
const ID_PRIMARIES: u32 = 0x55BB;
const ID_AUDIO: u32 = 0xE1;
const ID_SAMPLING_FREQUENCY: u32 = 0xB5;
const ID_CHANNELS: u32 = 0x9F;
const ID_TIMECODE: u32 = 0xE7;
const ID_SIMPLE_BLOCK: u32 = 0xA3;
const ID_BLOCK_GROUP: u32 = 0xA0;
const ID_BLOCK: u32 = 0xA1;
const ID_BLOCK_DURATION: u32 = 0x9B;
const ID_REFERENCE_BLOCK: u32 = 0xFB;

/// Elements that may appear inside a `Cluster`; anything else ends an unknown-sized one.
const CLUSTER_CHILDREN: [u32; 9] = [
    ID_TIMECODE,
    0x5854, // SilentTracks
    0xA7,   // Position
    0xAB,   // PrevSize
    ID_SIMPLE_BLOCK,
    ID_BLOCK_GROUP,
    0xAF, // EncryptedBlock
    0xEC, // Void
    0xBF, // CRC-32
];

/// What a chunk of WebM turned into.
pub(crate) enum WebmOutput {
    /// An initialization segment, as an MP4 `ftyp` + `moov` with one track (ID 1).
    Init(Vec<u8>),
    /// A media segment, as an MP4 `moof` + `mdat` for track 1.
    Fragment(Vec<u8>),
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Codec {
    Vp8,
    Vp9,
    Opus,
}

/// The track a WebM `SourceBuffer` plays.
#[derive(Clone, Debug)]
struct Track {
    number: u64,
    codec: Codec,
    /// MP4 timescale used for this track's samples.
    timescale: u32,
    /// Nanoseconds per frame, if the file says.
    default_duration: Option<u64>,
}

/// One frame of the cluster being read.
struct Frame {
    /// Presentation (= decode) time in nanoseconds.
    time: i64,
    /// Duration in nanoseconds, when the block gave one (shared by laced frames).
    duration: Option<u64>,
    keyframe: bool,
    data: Vec<u8>,
}

/// Incremental WebM parser: bytes in, MP4 init segments and fragments out.
pub(crate) struct WebmParser {
    /// Bytes not parsed yet (an element whose end hasn't arrived).
    pending: Vec<u8>,
    /// Bytes of an ignored element still to drop.
    skip: u64,
    /// Inside a `Cluster`: `Some(None)` for an unknown size, `Some(Some(n))` for `n` bytes left.
    cluster: Option<Option<u64>>,
    cluster_time: i64,
    timecode_scale: u64,
    track: Option<Track>,
    frames: Vec<Frame>,
    /// The last sample duration written, in track units: a guess for a cluster's final frame.
    last_duration: u32,
}

impl Default for WebmParser {
    fn default() -> Self {
        WebmParser {
            pending: vec![],
            skip: 0,
            cluster: None,
            cluster_time: 0,
            timecode_scale: 1_000_000,
            track: None,
            frames: vec![],
            last_duration: 0,
        }
    }
}

/// An EBML variable-length integer at `data[0..]`: (value, length). `None` if incomplete;
/// `Err` if invalid. With `keep_marker` (element IDs) the length marker bit stays in the value.
fn read_vint(data: &[u8], keep_marker: bool) -> Result<Option<(u64, usize)>, ()> {
    let Some(&first) = data.first() else {
        return Ok(None);
    };
    if first == 0 {
        return Err(());
    }
    let length = first.leading_zeros() as usize + 1;
    if data.len() < length {
        return Ok(None);
    }
    let mut value = if keep_marker {
        first as u64
    } else {
        (first as u64) & ((1u64 << (8 - length)) - 1)
    };
    for byte in &data[1..length] {
        value = (value << 8) | *byte as u64;
    }
    Ok(Some((value, length)))
}

/// An element header: (ID, payload size or `None` if unknown, header length).
fn read_header(data: &[u8]) -> Result<Option<(u32, Option<u64>, usize)>, ()> {
    let Some((id, id_length)) = read_vint(data, true)? else {
        return Ok(None);
    };
    if id_length > 4 {
        return Err(());
    }
    let Some((size, size_length)) = read_vint(&data[id_length..], false)? else {
        return Ok(None);
    };
    let unknown = size == (1u64 << (7 * size_length)) - 1;
    Ok(Some((
        id as u32,
        if unknown { None } else { Some(size) },
        id_length + size_length,
    )))
}

/// The complete child elements of `data`: (ID, payload).
fn children(data: &[u8]) -> Vec<(u32, &[u8])> {
    let mut out = vec![];
    let mut offset = 0;
    while let Ok(Some((id, Some(size), header))) = read_header(&data[offset..]) {
        let start = offset + header;
        let Some(end) = start
            .checked_add(size as usize)
            .filter(|end| *end <= data.len())
        else {
            break;
        };
        out.push((id, &data[start..end]));
        offset = end;
    }
    out
}

fn uint(data: &[u8]) -> u64 {
    data.iter()
        .take(8)
        .fold(0, |value, byte| (value << 8) | *byte as u64)
}

fn float(data: &[u8]) -> f64 {
    match data.len() {
        4 => f32::from_be_bytes(data.try_into().unwrap()) as f64,
        8 => f64::from_be_bytes(data.try_into().unwrap()),
        _ => 0.0,
    }
}

fn find(elements: &[(u32, &[u8])], id: u32) -> Option<u64> {
    elements
        .iter()
        .find(|(child, _)| *child == id)
        .map(|(_, data)| uint(data))
}

/// Duration of an Opus packet in 48 kHz samples, from its TOC byte (RFC 6716, 3.1).
fn opus_packet_samples(packet: &[u8]) -> Option<u32> {
    let toc = *packet.first()?;
    let config = toc >> 3;
    // Frame length in units of 2.5 ms (120 samples at 48 kHz).
    let units = match config {
        0..=11 => [4, 8, 16, 24][(config % 4) as usize],
        12..=15 => [4, 8][(config % 2) as usize],
        _ => [1, 2, 4, 8][(config % 4) as usize],
    };
    let frames = match toc & 3 {
        0 => 1,
        1 | 2 => 2,
        _ => (*packet.get(1)? & 0x3f) as u32,
    };
    Some(units * 120 * frames)
}

impl WebmParser {
    /// Parse newly appended bytes. `Err` means the data isn't usable WebM.
    pub(crate) fn append(&mut self, data: &[u8]) -> Result<Vec<WebmOutput>, ()> {
        let mut out = vec![];
        let mut data = data;
        if self.skip > 0 {
            let dropped = (self.skip as usize).min(data.len());
            self.skip -= dropped as u64;
            self.consume_cluster_bytes(dropped as u64, &mut out);
            data = &data[dropped..];
        }
        self.pending.extend_from_slice(data);
        let mut consumed = 0;
        let result = self.parse(&mut consumed, &mut out);
        self.pending.drain(..consumed);
        result?;
        // A cluster of unknown size may go on in the next append; its frames so far can play.
        if self.cluster == Some(None) {
            self.flush(&mut out);
        }
        Ok(out)
    }

    fn parse(&mut self, consumed: &mut usize, out: &mut Vec<WebmOutput>) -> Result<(), ()> {
        loop {
            let Some((id, size, header)) = read_header(&self.pending[*consumed..])? else {
                return Ok(());
            };
            let available = (self.pending.len() - *consumed) as u64;

            if self.cluster == Some(None) && !CLUSTER_CHILDREN.contains(&id) {
                // The end of a cluster of unknown size.
                self.flush(out);
                self.cluster = None;
            }

            match id {
                // A new initialization segment begins (e.g. a quality switch); its header is
                // skipped below.
                ID_EBML => {
                    self.flush(out);
                    self.cluster = None;
                },
                // Masters whose children are read one by one.
                ID_SEGMENT => {
                    *consumed += header;
                    continue;
                },
                ID_CLUSTER => {
                    self.flush(out);
                    *consumed += header;
                    self.cluster = match size {
                        Some(0) => None,
                        size => Some(size),
                    };
                    continue;
                },
                _ => {},
            }
            // Everything else is read (or skipped) whole.
            let Some(size) = size else {
                return Err(());
            };
            let total = header as u64 + size;
            let wanted = matches!(
                id,
                ID_INFO | ID_TRACKS | ID_TIMECODE | ID_SIMPLE_BLOCK | ID_BLOCK_GROUP
            );
            if !wanted {
                // Skip it, even if it hasn't all arrived yet.
                let present = available.min(total);
                *consumed += present as usize;
                self.skip = total - present;
                self.consume_cluster_bytes(present, out);
                if self.skip > 0 {
                    return Ok(());
                }
                continue;
            }
            if available < total {
                return Ok(());
            }
            let payload = self.pending[*consumed + header..*consumed + total as usize].to_vec();
            *consumed += total as usize;
            match id {
                ID_INFO => {
                    self.timecode_scale = find(&children(&payload), ID_TIMECODE_SCALE)
                        .filter(|scale| *scale > 0)
                        .unwrap_or(1_000_000);
                },
                ID_TRACKS => {
                    let init = self.parse_tracks(&payload)?;
                    out.push(WebmOutput::Init(init));
                },
                ID_TIMECODE => self.cluster_time = uint(&payload) as i64,
                ID_SIMPLE_BLOCK => {
                    let length = read_vint(&payload, false)?.ok_or(())?.1;
                    let keyframe = payload
                        .get(length + 2)
                        .is_some_and(|flags| flags & 0x80 != 0);
                    self.add_block(&payload, keyframe, None)?;
                },
                _ => {
                    let group = children(&payload);
                    let keyframe = !group.iter().any(|(id, _)| *id == ID_REFERENCE_BLOCK);
                    let duration = find(&group, ID_BLOCK_DURATION);
                    if let Some((_, block)) = group.iter().find(|(id, _)| *id == ID_BLOCK) {
                        self.add_block(block, keyframe, duration)?;
                    }
                },
            }
            self.consume_cluster_bytes(total, out);
        }
    }

    /// Account for `bytes` read inside a cluster of known size; flush it when it's complete.
    fn consume_cluster_bytes(&mut self, bytes: u64, out: &mut Vec<WebmOutput>) {
        if let Some(Some(left)) = self.cluster {
            let left = left.saturating_sub(bytes);
            if left == 0 {
                self.flush(out);
                self.cluster = None;
            } else {
                self.cluster = Some(Some(left));
            }
        }
    }

    fn parse_tracks(&mut self, payload: &[u8]) -> Result<Vec<u8>, ()> {
        self.flush(&mut vec![]);
        for (id, entry) in children(payload) {
            if id != ID_TRACK_ENTRY {
                continue;
            }
            let fields = children(entry);
            if fields.iter().any(|(id, _)| *id == ID_CONTENT_ENCODINGS) {
                // Encrypted or compressed frames.
                return Err(());
            }
            let codec_id = fields
                .iter()
                .find(|(id, _)| *id == ID_CODEC_ID)
                .map(|(_, data)| {
                    String::from_utf8_lossy(data)
                        .trim_end_matches('\0')
                        .to_owned()
                })
                .unwrap_or_default();
            let codec = match codec_id.as_str() {
                "V_VP8" => Codec::Vp8,
                "V_VP9" => Codec::Vp9,
                "A_OPUS" => Codec::Opus,
                _ => continue,
            };
            let track_type = find(&fields, ID_TRACK_TYPE).unwrap_or(0);
            if track_type != if codec == Codec::Opus { 2 } else { 1 } {
                continue;
            }
            let number = find(&fields, ID_TRACK_NUMBER).ok_or(())?;
            let private = fields
                .iter()
                .find(|(id, _)| *id == ID_CODEC_PRIVATE)
                .map(|(_, data)| *data)
                .unwrap_or_default();
            let default_duration = find(&fields, ID_DEFAULT_DURATION).filter(|value| *value > 0);

            let (timescale, sample_entry, handler, width, height) = match codec {
                Codec::Opus => {
                    let audio = fields
                        .iter()
                        .find(|(id, _)| *id == ID_AUDIO)
                        .map(|(_, data)| children(data))
                        .unwrap_or_default();
                    let channels = find(&audio, ID_CHANNELS).unwrap_or(2) as u16;
                    let rate = audio
                        .iter()
                        .find(|(id, _)| *id == ID_SAMPLING_FREQUENCY)
                        .map(|(_, data)| float(data))
                        .unwrap_or(48000.0);
                    (
                        48000,
                        opus_sample_entry(private, channels, rate as u32),
                        *b"soun",
                        0,
                        0,
                    )
                },
                Codec::Vp8 | Codec::Vp9 => {
                    let video = fields
                        .iter()
                        .find(|(id, _)| *id == ID_VIDEO)
                        .map(|(_, data)| children(data))
                        .unwrap_or_default();
                    let width = find(&video, ID_PIXEL_WIDTH).unwrap_or(0) as u16;
                    let height = find(&video, ID_PIXEL_HEIGHT).unwrap_or(0) as u16;
                    let colour = video
                        .iter()
                        .find(|(id, _)| *id == ID_COLOUR)
                        .map(|(_, data)| children(data))
                        .unwrap_or_default();
                    let entry = vp_sample_entry(codec, private, &colour, width, height);
                    (90000, entry, *b"vide", width, height)
                },
            };
            self.track = Some(Track {
                number,
                codec,
                timescale,
                default_duration,
            });
            self.last_duration = 0;
            return Ok(init_segment(
                &handler,
                timescale,
                &sample_entry,
                width,
                height,
            ));
        }
        // No track we can play.
        Err(())
    }

    /// Parse a `Block` / `SimpleBlock` payload and queue its frames.
    fn add_block(&mut self, block: &[u8], keyframe: bool, duration: Option<u64>) -> Result<(), ()> {
        let Some(track) = &self.track else {
            return Ok(());
        };
        let Some((number, length)) = read_vint(block, false)? else {
            return Err(());
        };
        if number != track.number {
            return Ok(());
        }
        let header = block.get(length..length + 3).ok_or(())?;
        let relative = i16::from_be_bytes([header[0], header[1]]) as i64;
        let flags = header[2];
        let body = &block[length + 3..];
        let time = (self.cluster_time + relative) * self.timecode_scale as i64;
        let duration = duration.map(|ticks| ticks * self.timecode_scale);

        let frames = split_lace(body, (flags >> 1) & 3)?;
        let count = frames.len() as u64;
        for (index, frame) in frames.into_iter().enumerate() {
            // Laced frames share the block's time; spread them over its duration if known.
            let offset = duration
                .or(track.default_duration.map(|each| each * count))
                .map(|total| total * index as u64 / count)
                .unwrap_or(0);
            self.frames.push(Frame {
                time: time + offset as i64,
                duration: duration.map(|total| total / count),
                keyframe: keyframe || track.codec == Codec::Opus,
                data: frame.to_vec(),
            });
        }
        Ok(())
    }

    /// Turn the frames read so far into one MP4 fragment.
    fn flush(&mut self, out: &mut Vec<WebmOutput>) {
        let frames = std::mem::take(&mut self.frames);
        let (Some(track), false) = (&self.track, frames.is_empty()) else {
            return;
        };
        let timescale = track.timescale as i128;
        let to_units =
            |nanos: i64| ((nanos as i128 * timescale + 500_000_000) / 1_000_000_000) as i64;
        let times: Vec<i64> = frames
            .iter()
            .map(|frame| to_units(frame.time).max(0))
            .collect();
        let mut samples = Vec::with_capacity(frames.len());
        for (index, frame) in frames.iter().enumerate() {
            let duration = match times.get(index + 1) {
                Some(next) => (next - times[index]).max(0) as u32,
                None => frame
                    .duration
                    .or(track.default_duration)
                    .map(|nanos| to_units(nanos as i64) as u32)
                    .or_else(|| match track.codec {
                        Codec::Opus => opus_packet_samples(&frame.data),
                        _ => None,
                    })
                    .unwrap_or(self.last_duration),
            };
            self.last_duration = duration;
            samples.push(Sample {
                duration,
                size: frame.data.len() as u32,
                flags: if frame.keyframe {
                    SYNC_SAMPLE_FLAGS
                } else {
                    NON_SYNC_SAMPLE_FLAGS
                },
                composition_offset: 0,
            });
        }
        let payloads: Vec<&[u8]> = frames.iter().map(|frame| frame.data.as_slice()).collect();
        out.push(WebmOutput::Fragment(write_chunk(
            &samples,
            &payloads,
            1,
            times[0] as u64,
            0,
        )));
    }
}

/// The frames of a block body given its lacing (0 none, 1 Xiph, 2 fixed-size, 3 EBML).
fn split_lace(body: &[u8], lacing: u8) -> Result<Vec<&[u8]>, ()> {
    if lacing == 0 {
        return Ok(vec![body]);
    }
    let count = *body.first().ok_or(())? as usize + 1;
    let mut offset = 1;
    let mut sizes = Vec::with_capacity(count);
    match lacing {
        1 => {
            for _ in 0..count - 1 {
                let mut size = 0;
                loop {
                    let byte = *body.get(offset).ok_or(())?;
                    offset += 1;
                    size += byte as usize;
                    if byte != 255 {
                        break;
                    }
                }
                sizes.push(size);
            }
        },
        3 => {
            let (first, length) = read_vint(&body[offset..], false)?.ok_or(())?;
            offset += length;
            sizes.push(first as usize);
            let mut previous = first as i64;
            for _ in 1..count - 1 {
                let (raw, length) = read_vint(&body[offset..], false)?.ok_or(())?;
                offset += length;
                let bias = (1i64 << (7 * length - 1)) - 1;
                previous += raw as i64 - bias;
                if previous < 0 {
                    return Err(());
                }
                sizes.push(previous as usize);
            }
        },
        _ => {
            let each = (body.len() - offset) / count;
            sizes = vec![each; count - 1];
        },
    }
    let mut frames = Vec::with_capacity(count);
    for size in sizes {
        let end = offset
            .checked_add(size)
            .filter(|end| *end <= body.len())
            .ok_or(())?;
        frames.push(&body[offset..end]);
        offset = end;
    }
    frames.push(&body[offset..]);
    Ok(frames)
}

fn full_box(out: &mut Vec<u8>, kind: &[u8; 4], version: u8, flags: u32, payload: &[u8]) {
    let mut data = vec![version];
    data.extend_from_slice(&flags.to_be_bytes()[1..]);
    data.extend_from_slice(payload);
    write_box(out, kind, &data);
}

/// The unity matrix of `mvhd` / `tkhd`.
fn matrix() -> Vec<u8> {
    let mut out = vec![];
    for value in [0x0001_0000u32, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000] {
        out.extend_from_slice(&value.to_be_bytes());
    }
    out
}

/// A `vp08` / `vp09` sample entry with its `vpcC` (VP codec configuration).
fn vp_sample_entry(
    codec: Codec,
    private: &[u8],
    colour: &[(u32, &[u8])],
    width: u16,
    height: u16,
) -> Vec<u8> {
    // VP9 CodecPrivate: ID (1 profile, 2 level, 3 bit depth, 4 chroma subsampling), length, value.
    let mut profile = 0;
    let mut level = 0;
    let mut bit_depth = find(colour, ID_BITS_PER_CHANNEL)
        .filter(|bits| *bits > 0)
        .unwrap_or(8) as u8;
    let mut chroma = 1; // 4:2:0 colocated with luma
    let mut offset = 0;
    while offset + 2 < private.len() {
        let (id, length) = (private[offset], private[offset + 1] as usize);
        let value = private[offset + 2];
        match id {
            1 => profile = value,
            2 => level = value,
            3 => bit_depth = value,
            4 => chroma = value,
            _ => {},
        }
        offset += 2 + length;
    }
    let full_range = find(colour, ID_RANGE) == Some(2);
    let primaries = find(colour, ID_PRIMARIES).unwrap_or(2) as u8;
    let transfer = find(colour, ID_TRANSFER_CHARACTERISTICS).unwrap_or(2) as u8;
    let matrix_coefficients = find(colour, ID_MATRIX_COEFFICIENTS).unwrap_or(2) as u8;

    let mut vpcc = vec![
        profile,
        level,
        (bit_depth << 4) | (chroma << 1) | full_range as u8,
        primaries,
        transfer,
        matrix_coefficients,
    ];
    vpcc.extend_from_slice(&0u16.to_be_bytes()); // no codec initialization data

    let mut entry = vec![0; 6];
    entry.extend_from_slice(&1u16.to_be_bytes()); // data reference index
    entry.extend_from_slice(&[0; 16]); // pre_defined, reserved
    entry.extend_from_slice(&width.to_be_bytes());
    entry.extend_from_slice(&height.to_be_bytes());
    entry.extend_from_slice(&0x0048_0000u32.to_be_bytes()); // 72 dpi
    entry.extend_from_slice(&0x0048_0000u32.to_be_bytes());
    entry.extend_from_slice(&[0; 4]);
    entry.extend_from_slice(&1u16.to_be_bytes()); // frame count
    entry.extend_from_slice(&[0; 32]); // compressor name
    entry.extend_from_slice(&0x0018u16.to_be_bytes()); // depth
    entry.extend_from_slice(&(-1i16).to_be_bytes());
    full_box(&mut entry, b"vpcC", 1, 0, &vpcc);

    let mut out = vec![];
    write_box(
        &mut out,
        if codec == Codec::Vp8 {
            b"vp08"
        } else {
            b"vp09"
        },
        &entry,
    );
    out
}

/// An `Opus` sample entry with its `dOps`, built from WebM's `OpusHead` CodecPrivate.
fn opus_sample_entry(private: &[u8], channels: u16, rate: u32) -> Vec<u8> {
    // OpusHead: magic (8), version, channels, pre-skip (LE), rate (LE), gain (LE), mapping...
    let head = private
        .strip_prefix(b"OpusHead")
        .filter(|head| head.len() >= 11);
    let channels = head.map(|head| head[1] as u16).unwrap_or(channels);
    let mut dops = vec![0, channels as u8];
    match head {
        Some(head) => {
            dops.extend_from_slice(&u16::from_le_bytes([head[2], head[3]]).to_be_bytes());
            dops.extend_from_slice(
                &u32::from_le_bytes([head[4], head[5], head[6], head[7]]).to_be_bytes(),
            );
            dops.extend_from_slice(&i16::from_le_bytes([head[8], head[9]]).to_be_bytes());
            dops.push(head[10]);
            if head[10] != 0 {
                // stream count, coupled count, channel mapping
                dops.extend_from_slice(&head[11..]);
            }
        },
        None => {
            dops.extend_from_slice(&312u16.to_be_bytes());
            dops.extend_from_slice(&rate.to_be_bytes());
            dops.extend_from_slice(&0i16.to_be_bytes());
            dops.push(0);
        },
    }

    let mut entry = vec![0; 6];
    entry.extend_from_slice(&1u16.to_be_bytes()); // data reference index
    entry.extend_from_slice(&[0; 8]);
    entry.extend_from_slice(&channels.to_be_bytes());
    entry.extend_from_slice(&16u16.to_be_bytes()); // sample size
    entry.extend_from_slice(&[0; 4]);
    entry.extend_from_slice(&(48000u32 << 16).to_be_bytes());
    write_box(&mut entry, b"dOps", &dops);

    let mut out = vec![];
    write_box(&mut out, b"Opus", &entry);
    out
}

/// `ftyp` + `moov` for one fragmented track (ID 1) with the given sample entry.
fn init_segment(
    handler: &[u8; 4],
    timescale: u32,
    sample_entry: &[u8],
    width: u16,
    height: u16,
) -> Vec<u8> {
    let is_video = handler == b"vide";

    let mut mvhd = vec![0; 8]; // creation, modification time
    mvhd.extend_from_slice(&1000u32.to_be_bytes()); // timescale
    mvhd.extend_from_slice(&0u32.to_be_bytes()); // duration
    mvhd.extend_from_slice(&0x0001_0000u32.to_be_bytes()); // rate
    mvhd.extend_from_slice(&0x0100u16.to_be_bytes()); // volume
    mvhd.extend_from_slice(&[0; 10]);
    mvhd.extend(matrix());
    mvhd.extend_from_slice(&[0; 24]);
    mvhd.extend_from_slice(&2u32.to_be_bytes()); // next track ID

    let mut tkhd = vec![0; 8];
    tkhd.extend_from_slice(&1u32.to_be_bytes()); // track ID
    tkhd.extend_from_slice(&[0; 4]);
    tkhd.extend_from_slice(&0u32.to_be_bytes()); // duration
    tkhd.extend_from_slice(&[0; 8]);
    tkhd.extend_from_slice(&[0; 4]); // layer, alternate group
    tkhd.extend_from_slice(&(if is_video { 0u16 } else { 0x0100 }).to_be_bytes());
    tkhd.extend_from_slice(&[0; 2]);
    tkhd.extend(matrix());
    tkhd.extend_from_slice(&((width as u32) << 16).to_be_bytes());
    tkhd.extend_from_slice(&((height as u32) << 16).to_be_bytes());

    let mut mdhd = vec![0; 8];
    mdhd.extend_from_slice(&timescale.to_be_bytes());
    mdhd.extend_from_slice(&0u32.to_be_bytes());
    mdhd.extend_from_slice(&0x55C4u16.to_be_bytes()); // "und"
    mdhd.extend_from_slice(&[0; 2]);

    let mut hdlr = vec![0; 4];
    hdlr.extend_from_slice(handler);
    hdlr.extend_from_slice(&[0; 13]);

    let mut stbl = vec![];
    let mut stsd = 1u32.to_be_bytes().to_vec();
    stsd.extend_from_slice(sample_entry);
    full_box(&mut stbl, b"stsd", 0, 0, &stsd);
    full_box(&mut stbl, b"stts", 0, 0, &[0; 4]);
    full_box(&mut stbl, b"stsc", 0, 0, &[0; 4]);
    full_box(&mut stbl, b"stsz", 0, 0, &[0; 8]);
    full_box(&mut stbl, b"stco", 0, 0, &[0; 4]);

    let mut dref = 1u32.to_be_bytes().to_vec();
    full_box(&mut dref, b"url ", 0, 1, &[]);
    let mut dinf = vec![];
    full_box(&mut dinf, b"dref", 0, 0, &dref);

    let mut minf = vec![];
    if is_video {
        full_box(&mut minf, b"vmhd", 0, 1, &[0; 8]);
    } else {
        full_box(&mut minf, b"smhd", 0, 0, &[0; 4]);
    }
    write_box(&mut minf, b"dinf", &dinf);
    write_box(&mut minf, b"stbl", &stbl);

    let mut mdia = vec![];
    full_box(&mut mdia, b"mdhd", 0, 0, &mdhd);
    full_box(&mut mdia, b"hdlr", 0, 0, &hdlr);
    write_box(&mut mdia, b"minf", &minf);

    let mut trak = vec![];
    full_box(&mut trak, b"tkhd", 0, 3, &tkhd);
    write_box(&mut trak, b"mdia", &mdia);

    let mut trex = 1u32.to_be_bytes().to_vec(); // track ID
    trex.extend_from_slice(&1u32.to_be_bytes()); // sample description index
    trex.extend_from_slice(&[0; 12]); // default duration, size, flags
    let mut mvex = vec![];
    full_box(&mut mvex, b"trex", 0, 0, &trex);

    let mut moov = vec![];
    full_box(&mut moov, b"mvhd", 0, 0, &mvhd);
    write_box(&mut moov, b"trak", &trak);
    write_box(&mut moov, b"mvex", &mvex);

    let mut out = vec![];
    write_box(&mut out, b"ftyp", b"iso6\0\0\0\0iso6mp41");
    write_box(&mut out, b"moov", &moov);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dom::fmp4::{TrackKind, parse_fragment, parse_init_segment, read_box};

    fn element(id: u32, payload: &[u8]) -> Vec<u8> {
        let mut out = id
            .to_be_bytes()
            .iter()
            .skip_while(|byte| **byte == 0)
            .copied()
            .collect::<Vec<u8>>();
        // 8-byte size vint.
        out.push(0x01);
        out.extend_from_slice(&(payload.len() as u64).to_be_bytes()[1..]);
        out.extend_from_slice(payload);
        out
    }

    fn init_bytes() -> Vec<u8> {
        let video = [
            element(ID_PIXEL_WIDTH, &[0x07, 0x80]),
            element(ID_PIXEL_HEIGHT, &[0x04, 0x38]),
        ]
        .concat();
        let entry = [
            element(ID_TRACK_NUMBER, &[1]),
            element(ID_TRACK_TYPE, &[1]),
            element(ID_CODEC_ID, b"V_VP9"),
            element(ID_VIDEO, &video),
        ]
        .concat();
        let info = element(ID_TIMECODE_SCALE, &[0x0F, 0x42, 0x40]);
        let mut out = element(ID_EBML, &element(0x4282, b"webm"));
        // Segment of unknown size.
        out.extend_from_slice(&[
            0x18, 0x53, 0x80, 0x67, 0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
        ]);
        out.extend(element(ID_INFO, &info));
        out.extend(element(ID_TRACKS, &element(ID_TRACK_ENTRY, &entry)));
        out
    }

    fn simple_block(relative: i16, keyframe: bool, data: &[u8]) -> Vec<u8> {
        let mut block = vec![0x81];
        block.extend_from_slice(&relative.to_be_bytes());
        block.push(if keyframe { 0x80 } else { 0 });
        block.extend_from_slice(data);
        element(ID_SIMPLE_BLOCK, &block)
    }

    fn cluster(time: u16) -> Vec<u8> {
        let body = [
            element(ID_TIMECODE, &time.to_be_bytes()),
            simple_block(0, true, b"key"),
            simple_block(33, false, b"d1"),
            simple_block(66, false, b"d2"),
        ]
        .concat();
        element(ID_CLUSTER, &body)
    }

    #[test]
    fn remuxes_init_and_clusters() {
        let mut parser = WebmParser::default();
        let mut stream = init_bytes();
        stream.extend(cluster(1000));
        stream.extend(cluster(1100));
        // Feed it in awkward pieces.
        let mut outputs = vec![];
        for piece in stream.chunks(7) {
            outputs.extend(parser.append(piece).unwrap());
        }
        assert_eq!(outputs.len(), 3);
        let WebmOutput::Init(init) = &outputs[0] else {
            panic!()
        };
        let init = parse_init_segment(init).unwrap();
        assert_eq!(init.tracks[0].kind, TrackKind::Video);
        assert_eq!(init.tracks[0].timescale, 90000);

        let WebmOutput::Fragment(fragment) = &outputs[1] else {
            panic!()
        };
        let moof = read_box(fragment, 0).unwrap().unwrap();
        let info = parse_fragment(fragment, &moof, 0, 0).unwrap();
        assert_eq!(info.decode_time, 90000);
        // 33 ms + 33 ms + (guessed) 33 ms.
        assert_eq!(info.duration, 3 * 2970);
        assert!(info.starts_with_sync);
    }

    #[test]
    fn splits_laces_and_reads_opus_durations() {
        let body = [2u8, 3, 255, 1, b'a', b'a', b'a'];
        let mut payload = body.to_vec();
        payload.extend(vec![b'b'; 256]);
        payload.extend_from_slice(b"cc");
        let frames = split_lace(&payload, 1).unwrap();
        assert_eq!(
            frames.iter().map(|frame| frame.len()).collect::<Vec<_>>(),
            vec![3, 256, 2]
        );
        // CELT 20 ms, one frame.
        assert_eq!(opus_packet_samples(&[0xF8]), Some(960));
        // SILK 60 ms, two frames.
        assert_eq!(opus_packet_samples(&[(3 << 3) | 1]), Some(5760));
    }
}
