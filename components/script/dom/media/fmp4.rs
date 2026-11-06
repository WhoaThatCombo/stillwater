/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Just enough ISO-BMFF (fragmented MP4) handling for Media Source Extensions.
//!
//! Sites append fragmented MP4 to each `SourceBuffer`: an *initialization segment*
//! (`ftyp` + `moov`) followed by *media segments* (`moof` + `mdat`, sometimes preceded by
//! `styp`/`sidx`/`emsg`). Servo's media backend plays one byte stream, so the audio and video
//! `SourceBuffer`s are merged here into a single two-track fragmented MP4 stream:
//!
//! - [`combined_init_segment`] builds one `moov` holding every track, renumbered 1..n.
//! - [`rewrite_fragment`] renumbers a fragment's track and shifts its decode time, so playback
//!   can restart (after a seek) from any fragment with the stream starting at time zero.
//!
//! Nothing here decodes media; boxes are copied byte for byte apart from those fields.

use std::ops::Range;

/// A complete box inside a buffer: its type and where it (and its payload) sit.
#[derive(Clone, Debug)]
pub(crate) struct BoxRef {
    pub(crate) kind: [u8; 4],
    /// The whole box, header included.
    pub(crate) range: Range<usize>,
    /// The payload, after the header.
    pub(crate) payload: Range<usize>,
}

/// Read the box starting at `offset`, if it is complete within `data`.
/// `Err(())` means the bytes can't be a box (corrupt input).
pub(crate) fn read_box(data: &[u8], offset: usize) -> Result<Option<BoxRef>, ()> {
    let Some(header) = data.get(offset..offset + 8) else {
        return Ok(None);
    };
    let size32 = u32::from_be_bytes([header[0], header[1], header[2], header[3]]) as u64;
    let kind = [header[4], header[5], header[6], header[7]];
    let (size, header_len) = match size32 {
        1 => {
            let Some(large) = data.get(offset + 8..offset + 16) else {
                return Ok(None);
            };
            (u64::from_be_bytes(large.try_into().unwrap()), 16)
        },
        // A size of zero means "to the end of the file", which never ends in a stream.
        0 => return Err(()),
        size => (size, 8),
    };
    if size < header_len as u64 {
        return Err(());
    }
    let end = offset as u64 + size;
    if end > data.len() as u64 {
        return Ok(None);
    }
    let end = end as usize;
    Ok(Some(BoxRef {
        kind,
        range: offset..end,
        payload: offset + header_len..end,
    }))
}

/// The complete child boxes of the payload `range` of `data`.
pub(crate) fn children(data: &[u8], range: Range<usize>) -> Vec<BoxRef> {
    let mut boxes = vec![];
    let mut offset = range.start;
    while offset < range.end {
        match read_box(&data[..range.end], offset) {
            Ok(Some(child)) => {
                offset = child.range.end;
                boxes.push(child);
            },
            _ => break,
        }
    }
    boxes
}

fn find<'a>(boxes: &'a [BoxRef], kind: &[u8; 4]) -> Option<&'a BoxRef> {
    boxes.iter().find(|child| &child.kind == kind)
}

fn be_u32(data: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_be_bytes(
        data.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

fn be_u64(data: &[u8], offset: usize) -> Option<u64> {
    Some(u64::from_be_bytes(
        data.get(offset..offset + 8)?.try_into().ok()?,
    ))
}

pub(crate) fn write_box(out: &mut Vec<u8>, kind: &[u8; 4], payload: &[u8]) {
    out.extend_from_slice(&((payload.len() + 8) as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(payload);
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum TrackKind {
    Video,
    Audio,
    Other,
}

/// One track of an initialization segment.
#[derive(Clone, Debug)]
pub(crate) struct TrackInfo {
    #[cfg_attr(not(test), expect(dead_code))]
    pub(crate) id: u32,
    pub(crate) kind: TrackKind,
    /// The sample entry type (`avc1`, `mp4a`, `vp09`, `Opus`, ...).
    pub(crate) codec: [u8; 4],
    /// Media time units per second (`mdhd`).
    pub(crate) timescale: u32,
    /// From `trex`: used when fragments don't give sample durations, sizes or flags.
    pub(crate) default_sample_duration: u32,
    pub(crate) default_sample_size: u32,
    pub(crate) default_sample_flags: u32,
    /// The `trak` box, verbatim.
    pub(crate) trak: Vec<u8>,
    /// The `trex` box, verbatim (synthesised if the file had none).
    pub(crate) trex: Vec<u8>,
}

/// A parsed initialization segment.
#[derive(Clone, Debug)]
pub(crate) struct InitSegment {
    pub(crate) ftyp: Vec<u8>,
    pub(crate) mvhd: Vec<u8>,
    /// The movie timescale (`mvhd`), which track header and edit list durations use.
    pub(crate) movie_timescale: u32,
    pub(crate) tracks: Vec<TrackInfo>,
    /// The segment's bytes, to notice when a site appends a different variant.
    pub(crate) raw: Vec<u8>,
}

/// Parse `ftyp` + `moov` (`data` holds exactly the boxes of the init segment).
pub(crate) fn parse_init_segment(data: &[u8]) -> Option<InitSegment> {
    let top = children(data, 0..data.len());
    let moov = find(&top, b"moov")?;
    let ftyp = find(&top, b"ftyp")
        .map(|ftyp| data[ftyp.range.clone()].to_vec())
        .unwrap_or_default();
    let moov_children = children(data, moov.payload.clone());
    let mvhd_box = find(&moov_children, b"mvhd")?;
    let mvhd = data[mvhd_box.range.clone()].to_vec();
    let mvhd_version = *data.get(mvhd_box.payload.start)?;
    let movie_timescale = be_u32(
        data,
        mvhd_box.payload.start + if mvhd_version == 1 { 4 + 16 } else { 4 + 8 },
    )?;

    let trexes: Vec<(u32, [u32; 3], Vec<u8>)> = find(&moov_children, b"mvex")
        .map(|mvex| {
            children(data, mvex.payload.clone())
                .into_iter()
                .filter(|child| &child.kind == b"trex")
                .filter_map(|trex| {
                    // full box header (4) + track_ID + description index + duration
                    let id = be_u32(data, trex.payload.start + 4)?;
                    let defaults = [
                        be_u32(data, trex.payload.start + 12)?,
                        be_u32(data, trex.payload.start + 16)?,
                        be_u32(data, trex.payload.start + 20)?,
                    ];
                    Some((id, defaults, data[trex.range.clone()].to_vec()))
                })
                .collect()
        })
        .unwrap_or_default();

    let mut tracks = vec![];
    for trak in moov_children.iter().filter(|child| &child.kind == b"trak") {
        let trak_children = children(data, trak.payload.clone());
        let tkhd = find(&trak_children, b"tkhd")?;
        let version = *data.get(tkhd.payload.start)?;
        let id_offset = tkhd.payload.start + if version == 1 { 4 + 16 } else { 4 + 8 };
        let id = be_u32(data, id_offset)?;

        let mdia = find(&trak_children, b"mdia")?;
        let mdia_children = children(data, mdia.payload.clone());
        let mdhd = find(&mdia_children, b"mdhd")?;
        let mdhd_version = *data.get(mdhd.payload.start)?;
        let timescale_offset = mdhd.payload.start + if mdhd_version == 1 { 4 + 16 } else { 4 + 8 };
        let timescale = be_u32(data, timescale_offset)?;
        let codec = find(&mdia_children, b"minf")
            .and_then(|minf| {
                let stbl = children(data, minf.payload.clone());
                let stbl = find(&stbl, b"stbl")?;
                let stsd = children(data, stbl.payload.clone());
                let stsd = find(&stsd, b"stsd")?;
                read_box(data, stsd.payload.start + 8).ok().flatten()
            })
            .map(|entry| entry.kind)
            .unwrap_or_default();
        let kind = match find(&mdia_children, b"hdlr")
            .and_then(|hdlr| data.get(hdlr.payload.start + 8..hdlr.payload.start + 12))
        {
            Some(b"vide") => TrackKind::Video,
            Some(b"soun") => TrackKind::Audio,
            _ => TrackKind::Other,
        };

        let (defaults, trex) = trexes
            .iter()
            .find(|(trex_id, ..)| *trex_id == id)
            .map(|(_, defaults, trex)| (*defaults, trex.clone()))
            .unwrap_or_else(|| {
                // version/flags, track_ID, sample description index 1, duration, size, flags
                let mut payload = vec![0; 4];
                payload.extend_from_slice(&id.to_be_bytes());
                payload.extend_from_slice(&1u32.to_be_bytes());
                payload.extend_from_slice(&[0; 12]);
                let mut trex = vec![];
                write_box(&mut trex, b"trex", &payload);
                ([0; 3], trex)
            });

        tracks.push(TrackInfo {
            id,
            kind,
            codec,
            timescale,
            default_sample_duration: defaults[0],
            default_sample_size: defaults[1],
            default_sample_flags: defaults[2],
            trak: data[trak.range.clone()].to_vec(),
            trex,
        });
    }
    if tracks.is_empty() {
        return None;
    }
    Some(InitSegment {
        ftyp,
        mvhd,
        movie_timescale,
        tracks,
        raw: data.to_vec(),
    })
}

/// Replace the track ID inside a verbatim `trak` (its `tkhd`) or `trex` box.
fn renumber(boxed: &[u8], new_id: u32) -> Vec<u8> {
    let mut out = boxed.to_vec();
    let Ok(Some(outer)) = read_box(&out, 0) else {
        return out;
    };
    let id_offset = match &outer.kind {
        b"trex" => Some(outer.payload.start + 4),
        b"trak" => children(&out, outer.payload.clone())
            .into_iter()
            .find(|child| &child.kind == b"tkhd")
            .and_then(|tkhd| {
                let version = *out.get(tkhd.payload.start)?;
                Some(tkhd.payload.start + if version == 1 { 4 + 16 } else { 4 + 8 })
            }),
        _ => None,
    };
    if let Some(offset) = id_offset &&
        offset + 4 <= out.len()
    {
        out[offset..offset + 4].copy_from_slice(&new_id.to_be_bytes());
    }
    out
}

/// Convert a `trak`'s movie-timescale fields (its `tkhd` duration and `elst` segment durations)
/// from timescale `from` to `to`, for use under another movie's `mvhd`.
fn rescale_movie_times(trak: &[u8], from: u32, to: u32) -> Vec<u8> {
    let mut out = trak.to_vec();
    if from == to || from == 0 {
        return out;
    }
    let rescale = |value: u64| -> u64 { (value as u128 * to as u128 / from as u128) as u64 };
    let Ok(Some(outer)) = read_box(trak, 0) else {
        return out;
    };
    for child in children(trak, outer.payload.clone()) {
        match &child.kind {
            b"tkhd" => {
                let version = trak[child.payload.start];
                if version == 1 {
                    let at = child.payload.start + 4 + 24;
                    if let Some(value) = be_u64(trak, at) {
                        out[at..at + 8].copy_from_slice(&rescale(value).to_be_bytes());
                    }
                } else {
                    let at = child.payload.start + 4 + 16;
                    if let Some(value) = be_u32(trak, at) {
                        let value = rescale(value as u64).min(u32::MAX as u64) as u32;
                        out[at..at + 4].copy_from_slice(&value.to_be_bytes());
                    }
                }
            },
            b"edts" => {
                for elst in children(trak, child.payload.clone()) {
                    if &elst.kind != b"elst" {
                        continue;
                    }
                    let version = trak[elst.payload.start];
                    let count = be_u32(trak, elst.payload.start + 4).unwrap_or(0) as usize;
                    let entry_len = if version == 1 { 20 } else { 12 };
                    for index in 0..count {
                        let at = elst.payload.start + 8 + index * entry_len;
                        if version == 1 {
                            if let Some(value) = be_u64(trak, at) {
                                out[at..at + 8].copy_from_slice(&rescale(value).to_be_bytes());
                            }
                        } else if let Some(value) = be_u32(trak, at) {
                            let value = rescale(value as u64).min(u32::MAX as u64) as u32;
                            out[at..at + 4].copy_from_slice(&value.to_be_bytes());
                        }
                    }
                }
            },
            _ => {},
        }
    }
    out
}

/// One `ftyp` + `moov` holding the given tracks, numbered 1, 2, … in order.
pub(crate) fn combined_init_segment(segments: &[(&InitSegment, &TrackInfo)]) -> Vec<u8> {
    let Some((first, _)) = segments.first() else {
        return vec![];
    };
    let mut moov_payload = first.mvhd.clone();
    // mvhd's next_track_ID is its last field: point it past our tracks.
    let next_id = segments.len() as u32 + 1;
    let length = moov_payload.len();
    if length >= 4 {
        moov_payload[length - 4..].copy_from_slice(&next_id.to_be_bytes());
    }
    let mut mvex_payload = vec![];
    for (index, (init, track)) in segments.iter().enumerate() {
        let id = index as u32 + 1;
        let trak = rescale_movie_times(&track.trak, init.movie_timescale, first.movie_timescale);
        moov_payload.extend(renumber(&trak, id));
        mvex_payload.extend(renumber(&track.trex, id));
    }
    write_box(&mut moov_payload, b"mvex", &mvex_payload);

    let mut out = first.ftyp.clone();
    write_box(&mut out, b"moov", &moov_payload);
    out
}

/// A media segment's timing, from its `moof`.
#[derive(Clone, Debug)]
pub(crate) struct FragmentInfo {
    #[cfg_attr(not(test), expect(dead_code))]
    pub(crate) track_id: u32,
    /// Decode time of the first sample, in the track's timescale.
    pub(crate) decode_time: u64,
    /// Total duration of its samples, in the track's timescale.
    pub(crate) duration: u64,
    /// Whether its first sample is a sync sample (a keyframe), so playback can start there.
    pub(crate) starts_with_sync: bool,
}

/// Timing of the `moof` at `moof` (the box itself) inside `data`.
pub(crate) fn parse_fragment(
    data: &[u8],
    moof: &BoxRef,
    default_duration: u32,
    default_flags: u32,
) -> Option<FragmentInfo> {
    let traf = children(data, moof.payload.clone())
        .into_iter()
        .find(|child| &child.kind == b"traf")?;
    let traf_children = children(data, traf.payload.clone());

    let tfhd = find(&traf_children, b"tfhd")?;
    let tfhd_flags = be_u32(data, tfhd.payload.start)? & 0x00ff_ffff;
    let track_id = be_u32(data, tfhd.payload.start + 4)?;
    let mut offset = tfhd.payload.start + 8;
    if tfhd_flags & 0x01 != 0 {
        offset += 8; // base data offset
    }
    if tfhd_flags & 0x02 != 0 {
        offset += 4; // sample description index
    }
    let mut sample_duration = default_duration;
    let mut sample_flags = default_flags;
    if tfhd_flags & 0x08 != 0 {
        sample_duration = be_u32(data, offset)?;
        offset += 4;
    }
    if tfhd_flags & 0x10 != 0 {
        offset += 4; // default sample size
    }
    if tfhd_flags & 0x20 != 0 {
        sample_flags = be_u32(data, offset)?;
    }

    let decode_time = match find(&traf_children, b"tfdt") {
        Some(tfdt) => {
            let version = *data.get(tfdt.payload.start)?;
            if version == 1 {
                be_u64(data, tfdt.payload.start + 4)?
            } else {
                be_u32(data, tfdt.payload.start + 4)? as u64
            }
        },
        None => 0,
    };

    let mut duration = 0u64;
    let mut first_flags = None;
    for trun in traf_children.iter().filter(|child| &child.kind == b"trun") {
        let flags = be_u32(data, trun.payload.start)? & 0x00ff_ffff;
        let count = be_u32(data, trun.payload.start + 4)?;
        let mut cursor = trun.payload.start + 8;
        if flags & 0x01 != 0 {
            cursor += 4; // data offset
        }
        if flags & 0x04 != 0 {
            first_flags.get_or_insert(be_u32(data, cursor)?);
            cursor += 4; // first sample flags
        }
        if count > 0 && flags & 0x400 != 0 {
            // The first sample's own flags follow its duration and size, when present.
            let at = cursor +
                if flags & 0x100 != 0 { 4 } else { 0 } +
                if flags & 0x200 != 0 { 4 } else { 0 };
            first_flags.get_or_insert(be_u32(data, at)?);
        }
        if count > 0 {
            first_flags.get_or_insert(sample_flags);
        }
        let per_sample = [0x100, 0x200, 0x400, 0x800]
            .iter()
            .filter(|bit| flags & **bit != 0)
            .count() *
            4;
        if flags & 0x100 != 0 {
            for _ in 0..count {
                duration += be_u32(data, cursor)? as u64;
                cursor += per_sample;
            }
        } else {
            duration += count as u64 * sample_duration as u64;
        }
    }

    Some(FragmentInfo {
        track_id,
        decode_time,
        duration,
        // `sample_is_non_sync_sample` clear: playback can start here.
        starts_with_sync: first_flags.is_none_or(|flags| flags & 0x0001_0000 == 0),
    })
}

/// One sample of a fragment, with everything needed to write it into a new `trun`.
pub(crate) struct Sample {
    pub(crate) duration: u32,
    pub(crate) size: u32,
    pub(crate) flags: u32,
    pub(crate) composition_offset: u32,
}

/// `trun` sample flags of a sync sample (depends on no other sample).
pub(crate) const SYNC_SAMPLE_FLAGS: u32 = 0x0200_0000;
/// `trun` sample flags of a non-sync sample (depends on others, `sample_is_non_sync_sample`).
pub(crate) const NON_SYNC_SAMPLE_FLAGS: u32 = 0x0101_0000;

/// A piece of a split fragment: a self-contained `moof` + `mdat`, ready to feed.
pub(crate) struct Chunk {
    /// Decode time of its first sample after the shift, in track timescale units.
    pub(crate) decode_time: u64,
    pub(crate) bytes: Vec<u8>,
}

/// Offset of the `mfhd` sequence number in a chunk made by [`split_fragment`].
const CHUNK_SEQUENCE_OFFSET: usize = 8 + 8 + 4;

/// Set the sequence number of a chunk made by [`split_fragment`].
pub(crate) fn set_chunk_sequence(chunk: &mut [u8], sequence: u32) {
    if chunk.len() >= CHUNK_SEQUENCE_OFFSET + 4 {
        chunk[CHUNK_SEQUENCE_OFFSET..CHUNK_SEQUENCE_OFFSET + 4]
            .copy_from_slice(&sequence.to_be_bytes());
    }
}

/// The samples of one `traf` of a media segment.
struct TrafSamples {
    track_id: u32,
    /// Decode time of the first sample, in the track's timescale.
    decode_time: u64,
    samples: Vec<Sample>,
    /// Where each sample's data is in the segment.
    ranges: Vec<Range<usize>>,
    trun_version: u8,
}

/// Every `traf` of the `moof` in `data` (a `moof` + `mdat`), with its samples. `track_for` gives
/// the `trex` defaults of a track ID; trafs of unknown tracks are left out.
fn parse_trafs<'a>(
    data: &[u8],
    track_for: &dyn Fn(u32) -> Option<&'a TrackInfo>,
) -> Option<Vec<TrafSamples>> {
    let top = children(data, 0..data.len());
    let moof = find(&top, b"moof")?;
    let mdat_start = find(&top, b"mdat")?.payload.start;
    // Without explicit offsets, each traf's data follows the previous one's.
    let mut next_data = mdat_start;
    let mut traf_index = 0;
    let mut trafs = vec![];
    for traf in children(data, moof.payload.clone())
        .into_iter()
        .filter(|child| &child.kind == b"traf")
    {
        let traf_children = children(data, traf.payload.clone());
        let tfhd = find(&traf_children, b"tfhd")?;
        let tfhd_flags = be_u32(data, tfhd.payload.start)? & 0x00ff_ffff;
        let track_id = be_u32(data, tfhd.payload.start + 4)?;
        let mut cursor = tfhd.payload.start + 8;
        // An absolute base offset can't be resolved within a segment; treat it as the moof
        // (default-base-is-moof), which is what segments for MSE use.
        let base = moof.range.start;
        let explicit_base = tfhd_flags & (0x01 | 0x02_0000) != 0;
        if tfhd_flags & 0x01 != 0 {
            cursor += 8;
        }
        if tfhd_flags & 0x02 != 0 {
            cursor += 4;
        }
        let track = track_for(track_id);
        let mut default_duration = track.map_or(0, |track| track.default_sample_duration);
        let mut default_size = track.map_or(0, |track| track.default_sample_size);
        let mut default_flags = track.map_or(0, |track| track.default_sample_flags);
        if tfhd_flags & 0x08 != 0 {
            default_duration = be_u32(data, cursor)?;
            cursor += 4;
        }
        if tfhd_flags & 0x10 != 0 {
            default_size = be_u32(data, cursor)?;
            cursor += 4;
        }
        if tfhd_flags & 0x20 != 0 {
            default_flags = be_u32(data, cursor)?;
        }

        let decode_time = match find(&traf_children, b"tfdt") {
            Some(tfdt) if data.get(tfdt.payload.start) == Some(&1) => {
                be_u64(data, tfdt.payload.start + 4)?
            },
            Some(tfdt) => be_u32(data, tfdt.payload.start + 4)? as u64,
            None => 0,
        };

        // Collect every sample and the bytes it occupies.
        let mut samples = vec![];
        let mut ranges: Vec<Range<usize>> = vec![];
        let mut trun_version = 0;
        // Relative to the base: the moof, or for a later traf without an explicit base, the
        // end of the previous traf's data (ISO/IEC 14496-12, 8.8.7.1).
        let traf_base = if explicit_base || traf_index == 0 {
            base
        } else {
            next_data
        };
        traf_index += 1;
        for trun in traf_children.iter().filter(|child| &child.kind == b"trun") {
            let version = *data.get(trun.payload.start)?;
            trun_version = trun_version.max(version);
            let flags = be_u32(data, trun.payload.start)? & 0x00ff_ffff;
            let count = be_u32(data, trun.payload.start + 4)?;
            let mut at = trun.payload.start + 8;
            let mut offset = next_data;
            if flags & 0x01 != 0 {
                let relative = be_u32(data, at)? as i32;
                offset = (traf_base as i64 + relative as i64) as usize;
                at += 4;
            }
            let mut first_flags = None;
            if flags & 0x04 != 0 {
                first_flags = Some(be_u32(data, at)?);
                at += 4;
            }
            for index in 0..count {
                let mut sample = Sample {
                    duration: default_duration,
                    size: default_size,
                    flags: if index == 0 {
                        first_flags.unwrap_or(default_flags)
                    } else {
                        default_flags
                    },
                    composition_offset: 0,
                };
                if flags & 0x100 != 0 {
                    sample.duration = be_u32(data, at)?;
                    at += 4;
                }
                if flags & 0x200 != 0 {
                    sample.size = be_u32(data, at)?;
                    at += 4;
                }
                if flags & 0x400 != 0 {
                    sample.flags = be_u32(data, at)?;
                    at += 4;
                }
                if flags & 0x800 != 0 {
                    sample.composition_offset = be_u32(data, at)?;
                    at += 4;
                }
                let end = offset + sample.size as usize;
                if end > data.len() {
                    return None;
                }
                ranges.push(offset..end);
                offset = end;
                samples.push(sample);
            }
            next_data = offset;
        }
        if track.is_some() {
            trafs.push(TrafSamples {
                track_id,
                decode_time,
                samples,
                ranges,
                trun_version,
            });
        }
    }
    Some(trafs)
}

/// Split a media segment (`moof` + `mdat`) holding several tracks (a muxed `SourceBuffer`)
/// into one segment per track: (track ID, `moof` + `mdat`). A one-track segment is returned
/// as is. `track_for` gives a track ID's `trex` defaults; unknown tracks are dropped.
pub(crate) fn split_tracks<'a>(
    data: Vec<u8>,
    track_for: &dyn Fn(u32) -> Option<&'a TrackInfo>,
) -> Option<Vec<(u32, Vec<u8>)>> {
    let top = children(&data, 0..data.len());
    let moof = find(&top, b"moof")?;
    let traf_ids: Vec<u32> = children(&data, moof.payload.clone())
        .into_iter()
        .filter(|child| &child.kind == b"traf")
        .filter_map(|traf| {
            let tfhd = find(&children(&data, traf.payload.clone()), b"tfhd")?.clone();
            be_u32(&data, tfhd.payload.start + 4)
        })
        .collect();
    if let [id] = traf_ids[..] {
        return Some(vec![(id, data)]);
    }
    let trafs = parse_trafs(&data, track_for)?;
    Some(
        trafs
            .into_iter()
            .filter(|traf| !traf.samples.is_empty())
            .map(|traf| {
                let payloads: Vec<&[u8]> = traf
                    .ranges
                    .iter()
                    .map(|range| &data[range.clone()])
                    .collect();
                (
                    traf.track_id,
                    write_chunk(
                        &traf.samples,
                        &payloads,
                        traf.track_id,
                        traf.decode_time,
                        traf.trun_version,
                    ),
                )
            })
            .collect(),
    )
}

/// Re-cut a media segment (`moof` + `mdat`, in `data`) into self-contained pieces of at most
/// `max_duration` (track timescale units), with its track renumbered to `new_id` and its decode
/// time moved back by `shift`.
///
/// Sites append several seconds of one track per segment; fed as-is, a player gets seconds of
/// video before any audio and its queues deadlock while starting. Small pieces of each track
/// can be interleaved by time, like an ordinary muxed file.
pub(crate) fn split_fragment(
    data: &[u8],
    track: &TrackInfo,
    new_id: u32,
    shift: u64,
    max_duration: u64,
) -> Option<Vec<Chunk>> {
    let trafs = parse_trafs(data, &|_| Some(track))?;
    // Stored fragments hold one track (see `split_tracks`).
    let Some(TrafSamples {
        decode_time,
        samples,
        ranges: sample_data,
        trun_version,
        ..
    }) = trafs.into_iter().next()
    else {
        return Some(vec![]);
    };
    if samples.is_empty() {
        return Some(vec![]);
    }

    // Group samples into pieces of at most `max_duration`.
    let mut chunks = vec![];
    let mut time = decode_time.saturating_sub(shift);
    let mut start = 0;
    while start < samples.len() {
        let mut end = start;
        let mut duration = 0u64;
        while end < samples.len() &&
            (end == start || duration + samples[end].duration as u64 <= max_duration)
        {
            duration += samples[end].duration as u64;
            end += 1;
        }
        let payloads: Vec<&[u8]> = sample_data[start..end]
            .iter()
            .map(|range| &data[range.clone()])
            .collect();
        chunks.push(Chunk {
            decode_time: time,
            bytes: write_chunk(&samples[start..end], &payloads, new_id, time, trun_version),
        });
        time += duration;
        start = end;
    }
    Some(chunks)
}

/// Write `samples` as one `moof` (mfhd, traf: tfhd, tfdt, trun) followed by their `mdat`
/// (`payloads`, one per sample).
pub(crate) fn write_chunk(
    samples: &[Sample],
    payloads: &[&[u8]],
    track_id: u32,
    decode_time: u64,
    trun_version: u8,
) -> Vec<u8> {
    let has_composition_offsets = samples.iter().any(|sample| sample.composition_offset != 0);
    let trun_flags: u32 =
        0x001 | 0x100 | 0x200 | 0x400 | if has_composition_offsets { 0x800 } else { 0 };
    let per_sample = if has_composition_offsets { 16 } else { 12 };

    // Sizes are fixed, so the data offset can be computed before writing.
    let mfhd_len = 8 + 4 + 4;
    let tfhd_len = 8 + 4 + 4;
    let tfdt_len = 8 + 4 + 8;
    let trun_len = 8 + 4 + 4 + 4 + samples.len() * per_sample;
    let traf_len = 8 + tfhd_len + tfdt_len + trun_len;
    let moof_len = 8 + mfhd_len + traf_len;
    let data_offset = (moof_len + 8) as u32;

    let mut trun = vec![trun_version];
    trun.extend_from_slice(&trun_flags.to_be_bytes()[1..]);
    trun.extend_from_slice(&(samples.len() as u32).to_be_bytes());
    trun.extend_from_slice(&data_offset.to_be_bytes());
    for sample in samples {
        trun.extend_from_slice(&sample.duration.to_be_bytes());
        trun.extend_from_slice(&sample.size.to_be_bytes());
        trun.extend_from_slice(&sample.flags.to_be_bytes());
        if has_composition_offsets {
            trun.extend_from_slice(&sample.composition_offset.to_be_bytes());
        }
    }
    // tfhd: default-base-is-moof, so trun's data offset counts from this moof.
    let mut tfhd = vec![0, 0x02, 0x00, 0x00];
    tfhd.extend_from_slice(&track_id.to_be_bytes());
    let mut tfdt = vec![1, 0, 0, 0];
    tfdt.extend_from_slice(&decode_time.to_be_bytes());

    let mut traf = vec![];
    write_box(&mut traf, b"tfhd", &tfhd);
    write_box(&mut traf, b"tfdt", &tfdt);
    write_box(&mut traf, b"trun", &trun);
    let mut moof = vec![];
    write_box(&mut moof, b"mfhd", &[0; 8]);
    write_box(&mut moof, b"traf", &traf);

    let mdat_len: usize = payloads.iter().map(|payload| payload.len()).sum();
    let mut out = Vec::with_capacity(moof_len + 8 + mdat_len);
    write_box(&mut out, b"moof", &moof);
    debug_assert_eq!(out.len(), moof_len);
    out.extend_from_slice(&((mdat_len + 8) as u32).to_be_bytes());
    out.extend_from_slice(b"mdat");
    for payload in payloads {
        out.extend_from_slice(payload);
    }
    out
}
#[cfg(test)]
mod tests {
    use super::*;

    fn boxed(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut out = vec![];
        write_box(&mut out, kind, payload);
        out
    }

    fn full(version: u8, flags: u32, rest: &[u8]) -> Vec<u8> {
        let mut out = vec![version];
        out.extend_from_slice(&flags.to_be_bytes()[1..]);
        out.extend_from_slice(rest);
        out
    }

    fn init(track_id: u32, handler: &[u8; 4], timescale: u32) -> Vec<u8> {
        let mut tkhd = vec![0; 8];
        tkhd.extend_from_slice(&track_id.to_be_bytes());
        tkhd.extend_from_slice(&[0; 68]);
        let mut mdhd = vec![0; 8];
        mdhd.extend_from_slice(&timescale.to_be_bytes());
        mdhd.extend_from_slice(&[0; 8]);
        let mut hdlr = vec![0; 4];
        hdlr.extend_from_slice(handler);
        hdlr.extend_from_slice(&[0; 13]);
        let mdia = [
            boxed(b"mdhd", &full(0, 0, &mdhd)),
            boxed(b"hdlr", &full(0, 0, &hdlr)),
        ]
        .concat();
        let trak = [boxed(b"tkhd", &full(0, 0, &tkhd)), boxed(b"mdia", &mdia)].concat();
        let mut trex = track_id.to_be_bytes().to_vec();
        trex.extend_from_slice(&1u32.to_be_bytes());
        trex.extend_from_slice(&1000u32.to_be_bytes());
        trex.extend_from_slice(&[0; 8]);
        let moov = [
            boxed(b"mvhd", &full(0, 0, &[0; 96])),
            boxed(b"trak", &trak),
            boxed(b"mvex", &boxed(b"trex", &full(0, 0, &trex))),
        ]
        .concat();
        [boxed(b"ftyp", b"iso5\0\0\0\0"), boxed(b"moov", &moov)].concat()
    }

    fn fragment(track_id: u32, decode_time: u64, durations: &[u32]) -> Vec<u8> {
        let tfhd = full(0, 0x020000, &track_id.to_be_bytes());
        let tfdt = full(1, 0, &decode_time.to_be_bytes());
        let mut trun = (durations.len() as u32).to_be_bytes().to_vec();
        for duration in durations {
            trun.extend_from_slice(&duration.to_be_bytes());
        }
        let traf = [
            boxed(b"tfhd", &tfhd),
            boxed(b"tfdt", &tfdt),
            boxed(b"trun", &full(0, 0x100, &trun)),
        ]
        .concat();
        let moof = [
            boxed(b"mfhd", &full(0, 0, &7u32.to_be_bytes())),
            boxed(b"traf", &traf),
        ]
        .concat();
        [boxed(b"moof", &moof), boxed(b"mdat", b"abcd")].concat()
    }

    #[test]
    fn parses_and_combines_init_segments() {
        let video = parse_init_segment(&init(1, b"vide", 90000)).unwrap();
        let audio = parse_init_segment(&init(1, b"soun", 48000)).unwrap();
        assert_eq!(video.tracks[0].kind, TrackKind::Video);
        assert_eq!(audio.tracks[0].timescale, 48000);
        assert_eq!(audio.tracks[0].default_sample_duration, 1000);

        let combined =
            combined_init_segment(&[(&video, &video.tracks[0]), (&audio, &audio.tracks[0])]);
        let parsed = parse_init_segment(&combined).unwrap();
        let ids: Vec<u32> = parsed.tracks.iter().map(|track| track.id).collect();
        assert_eq!(ids, vec![1, 2]);
        assert_eq!(parsed.tracks[1].kind, TrackKind::Audio);
    }

    /// A muxed media segment: track 1 with two 2-byte samples, track 2 with one 3-byte sample,
    /// data offsets relative to the moof.
    fn muxed_fragment() -> Vec<u8> {
        let traf = |track_id: u32, decode_time: u64, samples: &[(u32, u32)], data_offset: u32| {
            let tfhd = full(0, 0x020000, &track_id.to_be_bytes());
            let tfdt = full(1, 0, &decode_time.to_be_bytes());
            let mut trun = (samples.len() as u32).to_be_bytes().to_vec();
            trun.extend_from_slice(&data_offset.to_be_bytes());
            for (duration, size) in samples {
                trun.extend_from_slice(&duration.to_be_bytes());
                trun.extend_from_slice(&size.to_be_bytes());
            }
            [
                boxed(b"tfhd", &tfhd),
                boxed(b"tfdt", &tfdt),
                boxed(b"trun", &full(0, 0x301, &trun)),
            ]
            .concat()
        };
        // Offsets are fixed up below, once the moof's size is known.
        let build = |first: u32, second: u32| {
            let moof = [
                boxed(b"mfhd", &full(0, 0, &1u32.to_be_bytes())),
                boxed(b"traf", &traf(1, 9000, &[(3000, 2), (3000, 2)], first)),
                boxed(b"traf", &traf(2, 4800, &[(1024, 3)], second)),
            ]
            .concat();
            [boxed(b"moof", &moof), boxed(b"mdat", b"aabbccc")].concat()
        };
        let moof_len = read_box(&build(0, 0), 0).unwrap().unwrap().range.len() as u32;
        build(moof_len + 8, moof_len + 8 + 4)
    }

    #[test]
    fn splits_muxed_fragments_by_track() {
        let video = parse_init_segment(&init(1, b"vide", 90000)).unwrap();
        let audio = parse_init_segment(&init(2, b"soun", 48000)).unwrap();
        let tracks = [video.tracks[0].clone(), audio.tracks[0].clone()];
        let track_for = |id: u32| tracks.iter().find(|track| track.id == id);
        let split = split_tracks(muxed_fragment(), &track_for).unwrap();
        assert_eq!(
            split.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            vec![1, 2]
        );

        let (_, first) = &split[0];
        let moof = read_box(first, 0).unwrap().unwrap();
        let info = parse_fragment(first, &moof, 0, 0).unwrap();
        assert_eq!(
            (info.track_id, info.decode_time, info.duration),
            (1, 9000, 6000)
        );
        assert!(first.ends_with(b"aabb"));

        let (_, second) = &split[1];
        let moof = read_box(second, 0).unwrap().unwrap();
        let info = parse_fragment(second, &moof, 0, 0).unwrap();
        assert_eq!(
            (info.track_id, info.decode_time, info.duration),
            (2, 4800, 1024)
        );
        assert!(second.ends_with(b"ccc"));

        // Re-cutting a split segment for the feed keeps its samples and data.
        let chunks = split_fragment(second, &tracks[1], 2, 0, 48000).unwrap();
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].bytes.ends_with(b"ccc"));
    }

    #[test]
    fn reads_and_splits_fragments() {
        let video = parse_init_segment(&init(1, b"vide", 90000)).unwrap();
        let data = fragment(1, 180_000, &[3000, 3000, 3000]);
        let moof = read_box(&data, 0).unwrap().unwrap();
        let info = parse_fragment(&data, &moof, 0, 0).unwrap();
        assert_eq!(
            (info.track_id, info.decode_time, info.duration),
            (1, 180_000, 9000)
        );

        // Split into pieces of two samples at most, renumbered and shifted.
        let chunks = split_fragment(&data, &video.tracks[0], 2, 90_000, 6000).unwrap();
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].decode_time, 90_000);
        assert_eq!(chunks[1].decode_time, 96_000);
        let moof = read_box(&chunks[1].bytes, 0).unwrap().unwrap();
        let info = parse_fragment(&chunks[1].bytes, &moof, 0, 0).unwrap();
        assert_eq!(
            (info.track_id, info.decode_time, info.duration),
            (2, 96_000, 3000)
        );
    }
}
