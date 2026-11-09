/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Stillwater's on-device translation, in Rust.
//!
//! Runs the models Firefox Translations (Project Bergamot) publishes: a Marian transformer
//! encoder with an SSRU decoder, int8 weights, a SentencePiece vocabulary and a lexical shortlist.
//! Nothing leaves the computer; a language pair is three files, about 20 to 40 MB.
//!
//! Pages are translated a block at a time (a paragraph, a heading, a button), given as the runs
//! of text it's made of (its text nodes). Each translated word goes back into the run its source
//! word came from, using the model's word alignment, so links and emphasis stay in place.

mod model;
mod nn;
mod shortlist;
mod vocab;

use std::collections::HashMap;
use std::io::Result;
use std::path::Path;
use std::sync::Mutex;

use rayon::prelude::*;

use crate::nn::Network;
pub use crate::nn::profile::report as profile_report;
use crate::shortlist::Shortlist;
use crate::vocab::Vocab;

/// Longest piece of text, in source pieces, translated in one go; longer runs are split.
const MAX_SOURCE_PIECES: usize = 128;
/// Sentences decoded together at most, and source pieces per batch: decoding a batch reads the
/// model's weights once per step for all of them, instead of once per sentence.
const BATCH_SENTENCES: usize = 32;
const BATCH_PIECES: usize = 512;
/// Blocks remembered, so text a page shows again (or re-renders) isn't translated twice.
const CACHE_SIZE: usize = 20_000;

pub struct Translator {
    network: Network,
    /// Vocabulary of the language translated from (most pairs share one with the target).
    source_vocab: Vocab,
    /// Vocabulary of the language translated into.
    vocab: Vocab,
    shortlist: Option<Shortlist>,
    cache: Mutex<HashMap<Vec<String>, Vec<String>>>,
}

/// A translated sentence: the source pieces with their byte offsets in the sentence, and the
/// target pieces with the index of the source piece each is aligned to.
struct Translated {
    source: Vec<(u32, usize)>,
    words: Vec<(u32, usize)>,
}

impl Translator {
    /// Loads a language pair from its model (`model.*.bin`), vocabulary (`vocab.*.spm`, or a
    /// source and a target one: `srcvocab.*.spm`, `trgvocab.*.spm`) and, optionally, shortlist
    /// (`lex.*.bin`).
    pub fn from_bytes(
        model: &[u8],
        source_vocab: &[u8],
        target_vocab: Option<&[u8]>,
        shortlist: Option<&[u8]>,
    ) -> Result<Translator> {
        let network = Network::load(model)?;
        let source_vocab = Vocab::parse(source_vocab)?;
        let vocab = match target_vocab {
            Some(bytes) => Vocab::parse(bytes)?,
            None => Vocab::parse_shared(&source_vocab),
        };
        if vocab.len() != network.vocab_size() || source_vocab.len() > network.vocab_size() {
            return Err(model::invalid("vocabulary doesn't match the model"));
        }
        let shortlist = shortlist.map(Shortlist::parse).transpose()?;
        Ok(Translator {
            network,
            source_vocab,
            vocab,
            shortlist,
            cache: Mutex::new(HashMap::new()),
        })
    }

    /// Loads a language pair from a folder holding its three files.
    pub fn from_folder(folder: &Path) -> Result<Translator> {
        let find = |prefix: &str, suffix: &str| -> Option<std::path::PathBuf> {
            std::fs::read_dir(folder)
                .ok()?
                .flatten()
                .map(|entry| entry.path())
                .find(|path| {
                    path.file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.starts_with(prefix) && name.ends_with(suffix))
                })
        };
        let missing = |what: &str| model::invalid(format!("no {what} in {}", folder.display()));
        let model = std::fs::read(find("model.", ".bin").ok_or_else(|| missing("model.*.bin"))?)?;
        let shortlist = find("lex.", ".bin").map(std::fs::read).transpose()?;
        if let (Some(source), Some(target)) = (find("srcvocab.", ".spm"), find("trgvocab.", ".spm"))
        {
            let (source, target) = (std::fs::read(source)?, std::fs::read(target)?);
            return Translator::from_bytes(&model, &source, Some(&target), shortlist.as_deref());
        }
        let vocab = std::fs::read(find("vocab.", ".spm").ok_or_else(|| missing("vocab.*.spm"))?)?;
        Translator::from_bytes(&model, &vocab, None, shortlist.as_deref())
    }

    /// Translates plain texts, keeping the whitespace around and between sentences.
    pub fn translate(&self, texts: &[&str]) -> Vec<String> {
        let blocks: Vec<Vec<&str>> = texts.iter().map(|text| vec![*text]).collect();
        self.translate_blocks(&blocks)
            .into_iter()
            .map(|mut runs| runs.pop().unwrap_or_default())
            .collect()
    }

    /// Translates blocks of text, each given as its runs (text nodes, in order), and returns the
    /// translated runs. A run's translation is the words aligned to it, so a link's text stays
    /// in the link. Sentences are translated in parallel.
    pub fn translate_blocks(&self, blocks: &[Vec<&str>]) -> Vec<Vec<String>> {
        let keys: Vec<Vec<String>> = blocks
            .iter()
            .map(|runs| runs.iter().map(|run| run.to_string()).collect())
            .collect();
        let mut results: Vec<Option<Vec<String>>> = {
            let cache = self.cache.lock().unwrap_or_else(|error| error.into_inner());
            keys.iter().map(|key| cache.get(key).cloned()).collect()
        };
        let joined: Vec<String> = blocks.iter().map(|runs| runs.concat()).collect();
        let layouts: Vec<Vec<Segment>> = joined
            .iter()
            .enumerate()
            .map(|(index, text)| {
                if results[index].is_some() {
                    Vec::new()
                } else {
                    segments(text)
                }
            })
            .collect();
        // Every sentence of every block, in order, translated in parallel.
        let sentences: Vec<&str> = layouts
            .iter()
            .flatten()
            .filter_map(|segment| match *segment {
                Segment::Sentence { text, .. } => Some(text),
                Segment::Gap { .. } => None,
            })
            .collect();
        let translated = self.translate_sentences(&sentences);
        let mut translated = translated.into_iter();
        for (block, layout) in layouts.iter().enumerate() {
            if results[block].is_some() {
                continue;
            }
            let runs = &blocks[block];
            let mut starts = Vec::with_capacity(runs.len());
            let mut position = 0;
            for run in runs {
                starts.push(position);
                position += run.len();
            }
            // The run holding byte `at` of the joined text (an empty run never holds anything).
            let run_at = |at: usize| -> usize {
                starts
                    .partition_point(|&start| start <= at)
                    .saturating_sub(1)
            };
            let mut out = vec![String::new(); runs.len()];
            let mut last_written: Option<usize> = None;
            let mut after_sentence = false;
            for segment in layout {
                match *segment {
                    Segment::Gap { start, text } => {
                        for (offset, character) in text.char_indices() {
                            out[run_at(start + offset)].push(character);
                        }
                        after_sentence = false;
                    },
                    Segment::Sentence { start, .. } => {
                        let Some(sentence) = translated.next() else {
                            break;
                        };
                        let source_runs: Vec<usize> = sentence
                            .source
                            .iter()
                            .map(|&(_, offset)| run_at(start + offset))
                            .collect();
                        let placed = self.place(&sentence, &source_runs);
                        let mut chunks: Vec<(usize, Vec<u32>)> = Vec::new();
                        for (&(word, _), &run) in sentence.words.iter().zip(&placed) {
                            match chunks.last_mut() {
                                Some((last, ids)) if *last == run => ids.push(word),
                                _ => chunks.push((run, vec![word])),
                            }
                        }
                        for (index, (run, ids)) in chunks.iter().enumerate() {
                            let mut text = self.vocab.decode_raw(ids);
                            if index == 0 {
                                text = text.trim_start().to_owned();
                                // Chinese and Japanese sentences follow each other without a
                                // space; their translations may need one.
                                let before =
                                    last_written.map(|run| out[run].as_str()).unwrap_or("");
                                if after_sentence && needs_space(before, &text) {
                                    text.insert(0, ' ');
                                }
                            }
                            out[*run].push_str(&text);
                            last_written = Some(*run);
                        }
                        after_sentence = true;
                    },
                }
            }
            // Runs that touched without a space in a script written without spaces (Japanese
            // menu links: トップ社会経済) need one once translated ("Top Society Economy").
            for index in 1..out.len() {
                let source_joined = runs[index - 1]
                    .chars()
                    .next_back()
                    .zip(runs[index].chars().next())
                    .is_some_and(|(last, first)| {
                        !last.is_whitespace()
                            && !first.is_whitespace()
                            && (unspaced(last) || unspaced(first))
                    });
                if source_joined && needs_space(&out[index - 1], &out[index]) {
                    out[index].insert(0, ' ');
                }
            }
            results[block] = Some(out);
        }
        let mut cache = self.cache.lock().unwrap_or_else(|error| error.into_inner());
        if cache.len() > CACHE_SIZE {
            cache.clear();
        }
        keys.into_iter()
            .zip(results)
            .map(|(key, result)| {
                let result = result.unwrap_or_default();
                cache.entry(key).or_insert_with(|| result.clone());
                result
            })
            .collect()
    }

    /// Translates one sentence: its source pieces (with byte offsets) and the target pieces, each
    /// with the index of the source piece it's aligned to.
    fn translate_sentences(&self, sentences: &[&str]) -> Vec<Translated> {
        /// A sentence, or a chunk of a long one, to translate.
        struct Item {
            sentence: usize,
            /// Index of the chunk's first piece in the sentence.
            first: usize,
            ids: Vec<u32>,
        }
        let sources: Vec<Vec<(u32, usize)>> = sentences
            .par_iter()
            .map(|text| self.source_vocab.encode_with_offsets(text))
            .collect();
        let mut items = Vec::new();
        for (sentence, source) in sources.iter().enumerate() {
            for (chunk_index, chunk) in source.chunks(MAX_SOURCE_PIECES).enumerate() {
                let mut ids: Vec<u32> = chunk.iter().map(|&(id, _)| id).collect();
                ids.push(self.source_vocab.eos);
                items.push(Item {
                    sentence,
                    first: chunk_index * MAX_SOURCE_PIECES,
                    ids,
                });
            }
        }
        // Batches of similar length (they finish decoding about together), small enough that
        // every core gets work.
        let mut order: Vec<usize> = (0..items.len()).collect();
        order.sort_by_key(|&index| items[index].ids.len());
        // About four batches per core, so cores that finish early pick up more.
        let per_batch = items
            .len()
            .div_ceil(4 * rayon::current_num_threads().max(1))
            .clamp(1, BATCH_SENTENCES);
        let mut batches: Vec<Vec<usize>> = Vec::new();
        let mut pieces = 0;
        for index in order {
            let length = items[index].ids.len();
            match batches.last_mut() {
                Some(batch) if batch.len() < per_batch && pieces + length <= BATCH_PIECES => {
                    batch.push(index);
                    pieces += length;
                },
                _ => {
                    batches.push(vec![index]);
                    pieces = length;
                },
            }
        }
        let results: Vec<(usize, Vec<(u32, usize)>)> = batches
            .par_iter()
            .flat_map_iter(|batch| {
                let candidates: Vec<Vec<u32>> = batch
                    .iter()
                    .map(|&index| match &self.shortlist {
                        Some(shortlist) => {
                            shortlist.candidates(&items[index].ids, self.network.vocab_size())
                        },
                        None => Vec::new(),
                    })
                    .collect();
                let sources: Vec<&[u32]> = batch
                    .iter()
                    .map(|&index| items[index].ids.as_slice())
                    .collect();
                let candidates: Vec<&[u32]> = candidates.iter().map(Vec::as_slice).collect();
                let max_lengths: Vec<usize> = sources.iter().map(|ids| ids.len() * 2 + 4).collect();
                let words = self.network.translate_batch(
                    &sources,
                    &candidates,
                    self.vocab.eos,
                    1,
                    &max_lengths,
                );
                batch.clone().into_iter().zip(words)
            })
            .collect();
        let mut item_words: Vec<Vec<(u32, usize)>> = vec![Vec::new(); items.len()];
        for (index, words) in results {
            item_words[index] = words;
        }
        let mut translated: Vec<Translated> = sources
            .into_iter()
            .map(|source| Translated {
                source,
                words: Vec::new(),
            })
            .collect();
        // Items are in sentence order, chunks in order within each.
        for (item, words) in items.iter().zip(item_words) {
            let chunk_length = item.ids.len() - 1;
            let mut previous = item.first;
            for (word, aligned) in words {
                // A word aligned to the end-of-sentence marker goes with the one before it.
                let aligned = if aligned < chunk_length {
                    item.first + aligned
                } else {
                    previous
                };
                translated[item.sentence].words.push((word, aligned));
                previous = aligned;
            }
        }
        translated
    }

    /// Which run each target piece goes in, so that reading the runs in order reads the whole
    /// translation in order: the alignment's runs, with punctuation following the matching
    /// source punctuation, kept in non-decreasing order (the longest such sequence wins and the
    /// other pieces join their neighbours), and pieces inside a word staying with their word.
    fn place(&self, translated: &Translated, source_runs: &[usize]) -> Vec<usize> {
        let words = &translated.words;
        let mut runs: Vec<usize> = words
            .iter()
            .map(|&(word, aligned)| {
                if let Some(mark) = self.vocab.punctuation(word) {
                    // The same punctuation in the source, nearest the aligned piece.
                    let nearest = translated
                        .source
                        .iter()
                        .enumerate()
                        .filter(|&(_, &(id, _))| self.source_vocab.punctuation(id) == Some(mark))
                        .min_by_key(|&(index, _)| {
                            (index as isize - aligned as isize).unsigned_abs()
                        });
                    if let Some((index, _)) = nearest {
                        return source_runs[index];
                    }
                }
                source_runs.get(aligned).copied().unwrap_or(0)
            })
            .collect();
        // Longest non-decreasing subsequence (O(n²), n is a sentence).
        let count = runs.len();
        if count > 1 {
            let mut length = vec![1usize; count];
            let mut from = vec![usize::MAX; count];
            for i in 0..count {
                for j in 0..i {
                    if runs[j] <= runs[i] && length[j] + 1 > length[i] {
                        length[i] = length[j] + 1;
                        from[i] = j;
                    }
                }
            }
            let mut keep = vec![false; count];
            let mut at = (0..count).max_by_key(|&i| (length[i], i)).unwrap_or(0);
            while at != usize::MAX {
                keep[at] = true;
                at = from[at];
            }
            let first_kept = keep.iter().position(|&kept| kept).unwrap_or(0);
            let mut current = runs[first_kept];
            for i in 0..count {
                if keep[i] {
                    current = runs[i];
                } else {
                    runs[i] = current;
                }
            }
        }
        for i in 1..count {
            if self.vocab.continues_word(words[i].0) {
                runs[i] = runs[i - 1];
            }
        }
        runs
    }
}

/// A character of a script written without spaces between words (CJK, Thai, Lao, Burmese, Khmer).
fn unspaced(character: char) -> bool {
    matches!(character as u32,
        0x3000..=0x30ff | 0x3400..=0x4dbf | 0x4e00..=0x9fff | 0xf900..=0xfaff | 0xff00..=0xffef |
        0x0e00..=0x0e7f | 0x0e80..=0x0eff | 0x1000..=0x109f | 0x1780..=0x17ff)
}

/// Whether two translated sentences written one after the other need a space between them:
/// not when either side is in a script written without spaces.
fn needs_space(before: &str, after: &str) -> bool {
    match (before.chars().next_back(), after.chars().next()) {
        (Some(last), Some(first)) => {
            !last.is_whitespace() && !first.is_whitespace() && !unspaced(last) && !unspaced(first)
        },
        _ => false,
    }
}

#[derive(Debug, PartialEq)]
enum Segment<'a> {
    Sentence { start: usize, text: &'a str },
    Gap { start: usize, text: &'a str },
}

fn ends_sentence(character: char) -> bool {
    matches!(
        character,
        '.' | '!' | '?' | '…' | '。' | '！' | '？' | '؟' | '।'
    )
}

fn closes(character: char) -> bool {
    matches!(
        character,
        '"' | '\'' | ')' | ']' | '»' | '”' | '’' | '」' | '』' | '）'
    )
}

/// Abbreviations that end with a full stop but rarely end a sentence.
const ABBREVIATIONS: &[&str] = &[
    "Mr", "Mrs", "Ms", "Dr", "Prof", "St", "Sr", "Jr", "vs", "No", "Nr", "Co", "Inc", "Ltd", "Hr",
    "Fr", "bzw", "ca", "usw", "inkl", "ggf", "evtl", "vgl", "Jh", "Jhd", "Mio", "Mrd", "Sra", "Sr",
    "Dra", "Mme", "Mlle", "Av", "av", "Bd", "pp", "etc",
];

/// Splits text into sentences and the whitespace between them. A sentence ends at `.`, `!`, `?`
/// (and their CJK forms) plus any closing quotes, when whitespace and something other than a
/// lower-case letter follow (CJK needs no whitespace), or at a line break. A full stop after a
/// one- or two-digit number ("13. Jahrhundert"), an initial or a common abbreviation doesn't end
/// a sentence.
fn segments(text: &str) -> Vec<Segment<'_>> {
    let mut out = Vec::new();
    let characters: Vec<(usize, char)> = text.char_indices().collect();
    let mut index = 0;
    let mut sentence_start: Option<usize> = None;
    while index < characters.len() {
        let (at, character) = characters[index];
        if character.is_whitespace() {
            let gap_start = index;
            while index < characters.len() && characters[index].1.is_whitespace() {
                index += 1;
            }
            let gap_end = characters.get(index).map_or(text.len(), |&(at, _)| at);
            let gap = &text[at..gap_end];
            let mut back = gap_start;
            while back > 0 && closes(characters[back - 1].1) {
                back -= 1;
            }
            let ended = back > 0 && ends_sentence(characters[back - 1].1) && {
                let next_is_lower = characters
                    .get(index)
                    .is_some_and(|&(_, next)| next.is_lowercase());
                // The word before the full stop.
                let mut word_start = back - 1;
                while word_start > 0 && !characters[word_start - 1].1.is_whitespace() {
                    word_start -= 1;
                }
                let word: String = characters[word_start..back - 1]
                    .iter()
                    .map(|&(_, c)| c)
                    .collect();
                let not_an_end = characters[back - 1].1 == '.'
                    && ((!word.is_empty()
                        && word.len() <= 2
                        && word.chars().all(|c| c.is_ascii_digit()))
                        || (word.chars().count() == 1 && word.chars().all(char::is_alphabetic))
                        || ABBREVIATIONS.contains(&word.as_str()));
                !next_is_lower && !not_an_end
            };
            if sentence_start.is_none() || ended || gap.contains('\n') || index == characters.len()
            {
                if let Some(start) = sentence_start.take() {
                    out.push(Segment::Sentence {
                        start,
                        text: &text[start..at],
                    });
                }
                out.push(Segment::Gap {
                    start: at,
                    text: gap,
                });
            }
            continue;
        }
        if sentence_start.is_none() {
            sentence_start = Some(at);
        }
        // CJK full stops end a sentence without a following space.
        if matches!(character, '。' | '！' | '？') {
            let end = characters.get(index + 1).map_or(text.len(), |&(at, _)| at);
            let start = sentence_start.take().unwrap_or(at);
            out.push(Segment::Sentence {
                start,
                text: &text[start..end],
            });
        }
        index += 1;
    }
    if let Some(start) = sentence_start {
        out.push(Segment::Sentence {
            start,
            text: &text[start..],
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{Segment, segments};

    fn texts(text: &str) -> Vec<(bool, &str)> {
        segments(text)
            .into_iter()
            .map(|segment| match segment {
                Segment::Sentence { text, .. } => (true, text),
                Segment::Gap { text, .. } => (false, text),
            })
            .collect()
    }

    #[test]
    fn splits_sentences_and_keeps_whitespace() {
        assert_eq!(
            texts("  Hallo Welt. Wie geht's?\nGut"),
            vec![
                (false, "  "),
                (true, "Hallo Welt."),
                (false, " "),
                (true, "Wie geht's?"),
                (false, "\n"),
                (true, "Gut"),
            ]
        );
        assert_eq!(texts("z.B. drei Dinge"), vec![(true, "z.B. drei Dinge")]);
        assert_eq!(
            texts("Im 13. Jahrhundert, am 1. März"),
            vec![(true, "Im 13. Jahrhundert, am 1. März")]
        );
        assert_eq!(
            texts("J. R. R. Tolkien schrieb. Dr. Who"),
            vec![
                (true, "J. R. R. Tolkien schrieb."),
                (false, " "),
                (true, "Dr. Who"),
            ]
        );
        assert_eq!(
            texts("你好。再见。"),
            vec![(true, "你好。"), (true, "再见。")]
        );
    }

    #[test]
    fn offsets_point_into_the_text() {
        let text = "Eins. Zwei.";
        for segment in segments(text) {
            if let Segment::Sentence {
                start,
                text: sentence,
            } = segment
            {
                assert_eq!(&text[start..start + sentence.len()], sentence);
            }
        }
    }
}
