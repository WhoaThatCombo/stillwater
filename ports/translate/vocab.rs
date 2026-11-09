/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! A SentencePiece unigram tokenizer, reading the `.spm` model files Firefox Translations uses.
//!
//! Only what those models need: NFKC normalization, whitespace handling, Viterbi segmentation
//! over the piece scores, and byte fallback for characters without a piece.

use std::collections::HashMap;
use std::io::Result;

use icu_normalizer::ComposingNormalizerBorrowed;

use crate::model::invalid;

/// SentencePiece's word-boundary marker, which stands for a space.
const SPACE: char = '\u{2581}';
/// Score SentencePiece gives an unknown character, below every real piece.
const UNKNOWN_PENALTY: f32 = 10.0;

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Normal,
    Unknown,
    Control,
    UserDefined,
    Byte,
    Unused,
}

pub struct Vocab {
    pieces: Vec<(String, f32, Kind)>,
    lookup: HashMap<String, u32>,
    bytes: [u32; 256],
    longest: usize,
    min_score: f32,
    unknown: u32,
    pub eos: u32,
    add_dummy_prefix: bool,
    remove_extra_whitespaces: bool,
}

fn varint(bytes: &[u8], at: &mut usize) -> Result<u64> {
    let mut value = 0u64;
    let mut shift = 0;
    loop {
        let byte = *bytes
            .get(*at)
            .ok_or_else(|| invalid("vocabulary is truncated"))?;
        *at += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte < 0x80 {
            return Ok(value);
        }
        shift += 7;
        if shift > 63 {
            return Err(invalid("bad varint in vocabulary"));
        }
    }
}

enum Value<'a> {
    Int(u64),
    Bytes(&'a [u8]),
    Fixed32([u8; 4]),
}

/// The fields of one protobuf message, in order.
fn fields(bytes: &[u8]) -> Result<Vec<(u64, Value<'_>)>> {
    let mut at = 0;
    let mut out = Vec::new();
    while at < bytes.len() {
        let key = varint(bytes, &mut at)?;
        let value = match key & 7 {
            0 => Value::Int(varint(bytes, &mut at)?),
            1 => {
                at += 8;
                Value::Int(0)
            },
            2 => {
                let length = varint(bytes, &mut at)? as usize;
                let slice = bytes
                    .get(at..at + length)
                    .ok_or_else(|| invalid("vocabulary is truncated"))?;
                at += length;
                Value::Bytes(slice)
            },
            5 => {
                let slice = bytes
                    .get(at..at + 4)
                    .ok_or_else(|| invalid("vocabulary is truncated"))?;
                at += 4;
                Value::Fixed32(slice.try_into().unwrap())
            },
            other => return Err(invalid(format!("unsupported protobuf wire type {other}"))),
        };
        out.push((key >> 3, value));
    }
    Ok(out)
}

impl Vocab {
    pub fn parse(bytes: &[u8]) -> Result<Vocab> {
        let mut pieces = Vec::new();
        let mut add_dummy_prefix = true;
        let mut remove_extra_whitespaces = true;
        let mut byte_fallback = false;
        let mut unigram = true;
        for (field, value) in fields(bytes)? {
            match (field, value) {
                // ModelProto.pieces: { piece = 1, score = 2, type = 3 }
                (1, Value::Bytes(message)) => {
                    let mut text = String::new();
                    let mut score = 0.0;
                    let mut kind = Kind::Normal;
                    for (field, value) in fields(message)? {
                        match (field, value) {
                            (1, Value::Bytes(raw)) => {
                                text = String::from_utf8_lossy(raw).into_owned()
                            },
                            (2, Value::Fixed32(raw)) => score = f32::from_le_bytes(raw),
                            (3, Value::Int(value)) => {
                                kind = match value {
                                    2 => Kind::Unknown,
                                    3 => Kind::Control,
                                    4 => Kind::UserDefined,
                                    5 => Kind::Unused,
                                    6 => Kind::Byte,
                                    _ => Kind::Normal,
                                }
                            },
                            _ => {},
                        }
                    }
                    pieces.push((text, score, kind));
                },
                // ModelProto.trainer_spec: model_type = 3 (1 is unigram), byte_fallback = 35.
                (2, Value::Bytes(message)) => {
                    for (field, value) in fields(message)? {
                        match (field, value) {
                            (3, Value::Int(model_type)) => unigram = model_type == 1,
                            (35, Value::Int(flag)) => byte_fallback = flag != 0,
                            _ => {},
                        }
                    }
                },
                // ModelProto.normalizer_spec: add_dummy_prefix = 3, remove_extra_whitespaces = 4.
                (3, Value::Bytes(message)) => {
                    for (field, value) in fields(message)? {
                        match (field, value) {
                            (3, Value::Int(flag)) => add_dummy_prefix = flag != 0,
                            (4, Value::Int(flag)) => remove_extra_whitespaces = flag != 0,
                            _ => {},
                        }
                    }
                },
                _ => {},
            }
        }
        if !unigram {
            return Err(invalid("only unigram SentencePiece models are supported"));
        }
        if pieces.is_empty() {
            return Err(invalid("vocabulary has no pieces"));
        }
        let mut lookup = HashMap::new();
        let mut bytes_table = [u32::MAX; 256];
        let mut longest = 1;
        let mut min_score = f32::MAX;
        let mut unknown = 0;
        let mut eos = 0;
        for (id, (text, score, kind)) in pieces.iter().enumerate() {
            let id = id as u32;
            match kind {
                Kind::Normal | Kind::UserDefined => {
                    lookup.insert(text.clone(), id);
                    longest = longest.max(text.chars().count());
                    if *kind == Kind::Normal {
                        min_score = min_score.min(*score);
                    }
                },
                Kind::Byte if byte_fallback => {
                    if let Some(hex) = text
                        .strip_prefix("<0x")
                        .and_then(|rest| rest.strip_suffix('>'))
                    {
                        if let Ok(byte) = u8::from_str_radix(hex, 16) {
                            bytes_table[byte as usize] = id;
                        }
                    }
                },
                Kind::Unknown => unknown = id,
                Kind::Control if text == "</s>" => eos = id,
                _ => {},
            }
        }
        Ok(Vocab {
            pieces,
            lookup,
            bytes: bytes_table,
            longest,
            min_score,
            unknown,
            eos,
            add_dummy_prefix,
            remove_extra_whitespaces,
        })
    }

    /// A copy, for a pair whose source and target share one vocabulary.
    pub fn parse_shared(vocab: &Vocab) -> Vocab {
        Vocab {
            pieces: vocab.pieces.clone(),
            lookup: vocab.lookup.clone(),
            bytes: vocab.bytes,
            longest: vocab.longest,
            min_score: vocab.min_score,
            unknown: vocab.unknown,
            eos: vocab.eos,
            add_dummy_prefix: vocab.add_dummy_prefix,
            remove_extra_whitespaces: vocab.remove_extra_whitespaces,
        }
    }

    pub fn len(&self) -> usize {
        self.pieces.len()
    }

    /// SentencePiece's `nmt_nfkc` normalization (NFKC, control characters dropped, whitespace
    /// collapsed), then spaces turned into the word-boundary marker. Each character comes with
    /// the byte offset in `text` it came from.
    fn normalize(&self, text: &str) -> Vec<(char, usize)> {
        let nfkc = ComposingNormalizerBorrowed::new_nfkc();
        let mut out = Vec::with_capacity(text.len() + 1);
        if self.add_dummy_prefix {
            out.push((SPACE, 0));
        }
        let mut pending_space: Option<usize> = None;
        let mut started = false;
        let mut encoded = [0u8; 4];
        for (at, original) in text.char_indices() {
            if (original.is_control() && !original.is_whitespace())
                || matches!(original, '\u{200b}' | '\u{feff}' | '\u{ad}')
            {
                continue;
            }
            for character in nfkc.normalize(original.encode_utf8(&mut encoded)).chars() {
                if character.is_whitespace() {
                    if !self.remove_extra_whitespaces {
                        out.push((SPACE, at));
                    } else if started && pending_space.is_none() {
                        pending_space = Some(at);
                    }
                    continue;
                }
                if let Some(space_at) = pending_space.take() {
                    out.push((SPACE, space_at));
                }
                started = true;
                out.push((character, at));
            }
        }
        out
    }

    /// Splits `text` into the piece ids with the highest total score (Viterbi), each with the
    /// byte offset in `text` its text starts at (for a piece starting with a word-boundary marker,
    /// its first real character).
    pub fn encode_with_offsets(&self, text: &str) -> Vec<(u32, usize)> {
        let characters = self.normalize(text);
        let count = characters.len();
        if characters.iter().all(|&(character, _)| character == SPACE) {
            return Vec::new();
        }
        let normalized: String = characters.iter().map(|&(character, _)| character).collect();
        let mut byte_at = Vec::with_capacity(count + 1);
        let mut position = 0;
        for &(character, _) in &characters {
            byte_at.push(position);
            position += character.len_utf8();
        }
        byte_at.push(position);
        let offset = |index: usize| byte_at[index];
        // best[i]: best score of the first i characters, and the step that reached it.
        let mut best = vec![(f32::NEG_INFINITY, 0usize, u32::MAX); count + 1];
        best[0].0 = 0.0;
        for start in 0..count {
            let (score_so_far, ..) = best[start];
            if score_so_far == f32::NEG_INFINITY {
                continue;
            }
            let mut matched_single = false;
            for length in 1..=self.longest.min(count - start) {
                let piece = &normalized[offset(start)..offset(start + length)];
                if let Some(&id) = self.lookup.get(piece) {
                    let (_, score, kind) = &self.pieces[id as usize];
                    // User-defined symbols always win, like SentencePiece.
                    let score = if *kind == Kind::UserDefined {
                        0.0
                    } else {
                        *score
                    };
                    if length == 1 {
                        matched_single = true;
                    }
                    let total = score_so_far + score;
                    if total > best[start + length].0 {
                        best[start + length] = (total, start, id);
                    }
                }
            }
            if !matched_single {
                let total = score_so_far + self.min_score - UNKNOWN_PENALTY;
                if total > best[start + 1].0 {
                    best[start + 1] = (total, start, self.unknown);
                }
            }
        }
        let source_of = |start: usize, end: usize| -> usize {
            characters[start..end]
                .iter()
                .find(|&&(character, _)| character != SPACE)
                .unwrap_or(&characters[start])
                .1
        };
        let mut pieces = Vec::new();
        let mut end = count;
        while end > 0 {
            let (_, start, id) = best[end];
            let source = source_of(start, end);
            if id == self.unknown && self.bytes[0] != u32::MAX {
                // Byte fallback: the character's UTF-8 bytes, in reverse since we're unwinding.
                for byte in normalized[offset(start)..offset(end)].bytes().rev() {
                    pieces.push((self.bytes[byte as usize], source));
                }
            } else {
                pieces.push((id, source));
            }
            end = start;
        }
        pieces.reverse();
        pieces
    }

    /// Text of `ids` with spaces for word-boundary markers, a leading one included.
    pub fn decode_raw(&self, ids: &[u32]) -> String {
        let text = self.decode(ids);
        if self.add_dummy_prefix && ids.first().is_some_and(|&id| self.starts_with_space(id)) {
            format!(" {text}")
        } else {
            text
        }
    }

    /// The piece's text without its word-boundary marker, if it's only punctuation.
    pub fn punctuation(&self, id: u32) -> Option<&str> {
        let (text, _, kind) = self.pieces.get(id as usize)?;
        if *kind != Kind::Normal {
            return None;
        }
        let text = text.trim_start_matches(SPACE);
        (!text.is_empty() && text.chars().all(|character| !character.is_alphanumeric()))
            .then_some(text)
    }

    /// Whether the piece carries on the word before it (no word-boundary marker, not punctuation).
    pub fn continues_word(&self, id: u32) -> bool {
        self.pieces.get(id as usize).is_some_and(|(text, _, kind)| {
            *kind == Kind::Byte
                || (*kind == Kind::Normal
                    && !text.starts_with(SPACE)
                    && text.chars().next().is_some_and(char::is_alphanumeric))
        })
    }

    fn starts_with_space(&self, id: u32) -> bool {
        self.pieces
            .get(id as usize)
            .is_some_and(|(text, _, kind)| *kind == Kind::Normal && text.starts_with(SPACE))
    }

    pub fn decode(&self, ids: &[u32]) -> String {
        let mut bytes = Vec::new();
        for &id in ids {
            let Some((text, _, kind)) = self.pieces.get(id as usize) else {
                continue;
            };
            match kind {
                Kind::Byte => {
                    if let Some(byte) = text
                        .strip_prefix("<0x")
                        .and_then(|rest| rest.strip_suffix('>'))
                        .and_then(|hex| u8::from_str_radix(hex, 16).ok())
                    {
                        bytes.push(byte);
                    }
                },
                Kind::Normal | Kind::UserDefined => bytes.extend_from_slice(text.as_bytes()),
                Kind::Unknown => bytes.extend_from_slice(" \u{2047} ".as_bytes()),
                _ => {},
            }
        }
        let text = String::from_utf8_lossy(&bytes).replace(SPACE, " ");
        match text.strip_prefix(' ') {
            Some(rest) if self.add_dummy_prefix => rest.to_owned(),
            _ => text,
        }
    }
}
