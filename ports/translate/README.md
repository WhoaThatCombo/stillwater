# stillwater-translate

Stillwater's translation engine: it runs [Firefox Translations](https://github.com/mozilla/firefox-translations-models)'
models (Project Bergamot) on the CPU, in Rust, with no C++ and no WebAssembly. Firefox runs the
same models with Bergamot, a C++ build of the Marian translation toolkit compiled to WebAssembly;
this crate reads Marian's files and does the same arithmetic itself.

A language pair is three files Mozilla publishes:

- `model.<pair>.intgemm.alphas.bin`: Marian's binary model format. A transformer encoder and an SSRU
  decoder ("tiny"/"base" student models), with int8 weights stored transposed and their quantization
  multipliers, plus the multipliers for each layer's input ("alphas").
- `vocab.<pair>.spm` (or `srcvocab`/`trgvocab`): a SentencePiece unigram model.
- `lex.50.50.<pair>.s2t.bin`: Marian's binary lexical shortlist.

What the crate does:

| File | |
|---|---|
| `model.rs` | Reads Marian's binary format and the model's `special:model.yml`. |
| `vocab.rs` | SentencePiece: NFKC normalization, Viterbi segmentation, byte fallback, decoding. |
| `shortlist.rs` | The shortlist: which target pieces are worth scoring for a sentence. |
| `nn.rs` | The network: int8 matrix products (AVX2 when the CPU has it), attention, layer norm, the SSRU, greedy decoding, and the word alignment the models were trained to produce (first head of the last decoder layer). |
| `lib.rs` | Sentences, text runs and the cache: a block of text is translated as the runs it's made of (the text nodes of a paragraph), and each translated word goes back into the run its source word came from, so a link keeps its words. Runs keep their order, punctuation follows the matching source punctuation, and word pieces stay together. |

Try it outside the browser:

```powershell
cargo build --release -p stillwater-translate
"Die Bundesregierung hat ein neues Gesetz beschlossen." | target\release\sw-translate.exe <folder with the three files>
```

Tabs in a line split it into runs, which come back tab-separated. On one core of a desktop CPU a
sentence takes a few milliseconds; a long Wikipedia article (16,000 words) takes about 2.5 s on all
cores.
