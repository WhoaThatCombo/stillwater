/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! `sw-translate <language pair folder>`: translates each line of standard input and prints it,
//! with timings on standard error. Tabs split a line into runs (like the text nodes of a
//! paragraph with a link in it), which come back tab-separated. For trying models and the engine
//! outside the browser.

use std::io::{BufRead, Write};
use std::time::Instant;

use stillwater_translate::Translator;

const TAB: char = '\u{9}';

fn main() {
    let Some(folder) = std::env::args().nth(1) else {
        eprintln!("usage: sw-translate <folder with model.*.bin, vocab.*.spm, lex.*.bin>");
        std::process::exit(2);
    };
    let started = Instant::now();
    let translator = match Translator::from_folder(folder.as_ref()) {
        Ok(translator) => translator,
        Err(error) => {
            eprintln!("Can't load {folder}: {error}");
            std::process::exit(1);
        },
    };
    eprintln!("loaded in {:.2}s", started.elapsed().as_secs_f32());
    let lines: Vec<String> = std::io::stdin()
        .lock()
        .lines()
        .map_while(Result::ok)
        .collect();
    let blocks: Vec<Vec<&str>> = lines.iter().map(|line| line.split(TAB).collect()).collect();
    let started = Instant::now();
    let translated = translator.translate_blocks(&blocks);
    let seconds = started.elapsed().as_secs_f32();
    let mut out = std::io::stdout().lock();
    for runs in translated {
        let _ = writeln!(out, "{}", runs.join(&TAB.to_string()));
    }
    eprintln!("translated {} lines in {seconds:.2}s", blocks.len());
}
