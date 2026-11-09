/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Marian's binary lexical shortlist (`lex.*.s2t.bin`): for each source piece, the target pieces
//! it usually translates to. Scoring only those (plus the most common pieces) instead of the whole
//! vocabulary makes each decoding step several times cheaper.

use std::io::Result;

use crate::model::invalid;

const MAGIC: u64 = 0xF11A48D5013417F5;

pub struct Shortlist {
    first: u32,
    offsets: Vec<u64>,
    lists: Vec<u32>,
}

impl Shortlist {
    pub fn parse(bytes: &[u8]) -> Result<Shortlist> {
        let word = |index: usize| -> Result<u64> {
            bytes
                .get(index * 8..index * 8 + 8)
                .map(|raw| u64::from_le_bytes(raw.try_into().unwrap()))
                .ok_or_else(|| invalid("shortlist is truncated"))
        };
        // Header: magic, checksum, firstNum, bestNum, offsets length, lists length.
        if word(0)? != MAGIC {
            return Err(invalid("not a Marian binary shortlist"));
        }
        let first = word(2)? as u32;
        let offsets_length = word(4)? as usize;
        let lists_length = word(5)? as usize;
        let header = 6 * 8;
        let expected = header + offsets_length * 8 + lists_length * 4;
        if bytes.len() != expected {
            return Err(invalid("shortlist size doesn't match its header"));
        }
        let offsets = bytes[header..header + offsets_length * 8]
            .chunks_exact(8)
            .map(|raw| u64::from_le_bytes(raw.try_into().unwrap()))
            .collect::<Vec<_>>();
        let lists = bytes[header + offsets_length * 8..]
            .chunks_exact(4)
            .map(|raw| u32::from_le_bytes(raw.try_into().unwrap()))
            .collect::<Vec<_>>();
        if offsets.last().copied() != Some(lists_length as u64)
            || offsets.windows(2).any(|pair| pair[0] > pair[1])
        {
            return Err(invalid("shortlist offsets are inconsistent"));
        }
        Ok(Shortlist {
            first,
            offsets,
            lists,
        })
    }

    /// The target pieces worth scoring when translating `source`, sorted.
    pub fn candidates(&self, source: &[u32], vocab_size: usize) -> Vec<u32> {
        let mut wanted = vec![false; vocab_size];
        for id in 0..(self.first as usize).min(vocab_size) {
            wanted[id] = true;
        }
        for &word in source {
            let word = word as usize;
            if word + 1 >= self.offsets.len() {
                continue;
            }
            let (start, end) = (self.offsets[word] as usize, self.offsets[word + 1] as usize);
            for &target in &self.lists[start..end] {
                if let Some(slot) = wanted.get_mut(target as usize) {
                    *slot = true;
                }
            }
        }
        wanted
            .iter()
            .enumerate()
            .filter_map(|(id, &keep)| keep.then_some(id as u32))
            .collect()
    }
}
