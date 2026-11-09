/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! The translation network: a Marian transformer encoder and an SSRU decoder, as in Firefox
//! Translations' student models. Weights stay int8 as stored; each layer's input is quantized
//! with the multiplier saved in the model ("alphas"), as Bergamot does, and multiplied in i32.

use std::collections::HashMap;
use std::f32::consts::FRAC_PI_2;
use std::io::Result;

use crate::model::{Item, config, invalid};

/// Time spent per stage, when `STILLWATER_TRANSLATE_PROFILE` is set (for tuning).
pub mod profile {
    use std::sync::OnceLock;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Instant;

    pub const NAMES: [&str; 7] = [
        "encoder",
        "decoder rnn",
        "decoder attention",
        "decoder ffn",
        "output",
        "(quantize)",
        "(matmul)",
    ];
    pub static NANOS: [AtomicU64; 7] = [const { AtomicU64::new(0) }; 7];

    pub fn enabled() -> bool {
        static ON: OnceLock<bool> = OnceLock::new();
        *ON.get_or_init(|| std::env::var_os("STILLWATER_TRANSLATE_PROFILE").is_some())
    }

    pub fn start() -> Option<Instant> {
        enabled().then(Instant::now)
    }

    pub fn stop(stage: usize, started: Option<Instant>) {
        if let Some(started) = started {
            NANOS[stage].fetch_add(started.elapsed().as_nanos() as u64, Ordering::Relaxed);
        }
    }

    pub fn report() -> String {
        NAMES
            .iter()
            .zip(&NANOS)
            .map(|(name, nanos)| {
                format!("{name}: {:.2}s", nanos.load(Ordering::Relaxed) as f64 / 1e9)
            })
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// `y = x·W + b` with int8 `W` stored transposed (one row per output).
pub(crate) struct Linear {
    weights: Vec<i8>,
    inputs: usize,
    outputs: usize,
    weight_multiplier: f32,
    /// Multiplier for quantizing inputs; 0 means work it out per row.
    input_multiplier: f32,
    bias: Option<Vec<f32>>,
}

fn item<'a>(items: &'a HashMap<String, Item>, name: &str) -> Result<&'a Item> {
    items
        .get(name)
        .ok_or_else(|| invalid(format!("model has no {name}")))
}

impl Linear {
    fn load(items: &HashMap<String, Item>, name: &str, bias: Option<&str>) -> Result<Linear> {
        let weights_item = item(items, name)?;
        let (weights, weight_multiplier) = weights_item.int8()?;
        if weights_item.shape.len() != 2 {
            return Err(invalid(format!("{name} isn't a matrix")));
        }
        // Stored transposed: the shape is [inputs, outputs], the bytes [outputs][inputs]. The
        // embedding matrix is the exception, [vocabulary][dim], and is used as an output layer.
        let (inputs, outputs) = if name.ends_with("Wemb") {
            (weights_item.shape[1], weights_item.shape[0])
        } else {
            (weights_item.shape[0], weights_item.shape[1])
        };
        // Saved multipliers are float32 items. `Wemb_QuantMultA` is stored with the int8 tag and
        // holds no real value, so the output layer, like any layer without one, works out a
        // multiplier per row.
        let input_multiplier = match items.get(&format!("{name}_QuantMultA")) {
            Some(multiplier) if multiplier.ty == crate::model::TYPE_FLOAT32 => {
                let value = multiplier.scalar()?;
                if value.is_finite() && value > 0.0 {
                    value
                } else {
                    0.0
                }
            },
            _ => 0.0,
        };
        let bias = match bias {
            Some(bias) => {
                let values = item(items, bias)?.floats()?;
                if values.len() != outputs {
                    return Err(invalid(format!("{bias} has the wrong size")));
                }
                Some(values)
            },
            None => None,
        };
        Ok(Linear {
            weights,
            inputs,
            outputs,
            weight_multiplier,
            input_multiplier,
            bias,
        })
    }

    /// Quantizes one input row to int8 values (held as i16, ready for the multiply-add kernel).
    fn quantize(&self, row: &[f32], out: &mut [i16]) -> f32 {
        let multiplier = if self.input_multiplier > 0.0 {
            self.input_multiplier
        } else {
            let largest = row.iter().fold(0f32, |max, value| max.max(value.abs()));
            if largest > 0.0 { 127.0 / largest } else { 1.0 }
        };
        quantize_into(row, multiplier, out);
        multiplier
    }

    /// `rows` inputs, one after another, to `rows` outputs.
    pub(crate) fn forward(&self, input: &[f32], rows: usize) -> Vec<f32> {
        self.forward_columns(input, rows, &[])
    }

    /// Like `forward`, computing only the given outputs (all of them when `columns` is empty).
    pub(crate) fn forward_columns(&self, input: &[f32], rows: usize, columns: &[u32]) -> Vec<f32> {
        let width = if columns.is_empty() {
            self.outputs
        } else {
            columns.len()
        };
        let inputs = self.inputs;
        let started = profile::start();
        let mut quantized = vec![0i16; rows * inputs];
        let mut multipliers = Vec::with_capacity(rows);
        for row in 0..rows {
            multipliers.push(self.quantize(
                &input[row * inputs..(row + 1) * inputs],
                &mut quantized[row * inputs..(row + 1) * inputs],
            ));
        }
        profile::stop(5, started);
        let started = profile::start();
        let mut sums = vec![0i32; rows * width];
        matmul(
            &quantized,
            rows,
            inputs,
            &self.weights,
            columns,
            width,
            &mut sums,
        );
        profile::stop(6, started);
        let mut out = vec![0f32; rows * width];
        for row in 0..rows {
            let scale = 1.0 / (multipliers[row] * self.weight_multiplier);
            let sums = &sums[row * width..(row + 1) * width];
            let out_row = &mut out[row * width..(row + 1) * width];
            for (index, (value, &sum)) in out_row.iter_mut().zip(sums).enumerate() {
                *value = sum as f32 * scale;
                if let Some(bias) = &self.bias {
                    let column = if columns.is_empty() {
                        index
                    } else {
                        columns[index] as usize
                    };
                    *value += bias[column];
                }
            }
        }
        out
    }

    /// Row `index` of the weights as floats (the embedding of a word).
    fn row(&self, index: usize) -> impl Iterator<Item = f32> + '_ {
        let multiplier = 1.0 / self.weight_multiplier;
        self.weights[index * self.inputs..(index + 1) * self.inputs]
            .iter()
            .map(move |&value| value as f32 * multiplier)
    }
}

/// `out = clamp(round_ties_even(row · multiplier), −127, 127)`.
fn quantize_into(row: &[f32], multiplier: f32, out: &mut [i16]) {
    #[cfg(target_arch = "x86_64")]
    if std::arch::is_x86_feature_detected!("avx2") && out.len() >= row.len() {
        // Safety: the CPU supports AVX2 (checked) and `out` is as long as `row` (checked).
        unsafe { quantize_avx2(row, multiplier, out) };
        return;
    }
    for (slot, &value) in out.iter_mut().zip(row) {
        *slot = (value * multiplier).round_ties_even().clamp(-127.0, 127.0) as i16;
    }
}

/// `quantize_into` sixteen values at a time; the same results as the scalar version.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn quantize_avx2(row: &[f32], multiplier: f32, out: &mut [i16]) {
    use std::arch::x86_64::*;
    unsafe {
        let scale = _mm256_set1_ps(multiplier);
        let low = _mm256_set1_ps(-127.0);
        let high = _mm256_set1_ps(127.0);
        let convert = |at: usize| {
            let scaled = _mm256_mul_ps(_mm256_loadu_ps(row.as_ptr().add(at)), scale);
            let rounded =
                _mm256_round_ps::<{ _MM_FROUND_TO_NEAREST_INT | _MM_FROUND_NO_EXC }>(scaled);
            _mm256_cvtps_epi32(_mm256_min_ps(_mm256_max_ps(rounded, low), high))
        };
        let mut i = 0;
        while i + 16 <= row.len() {
            // packs works within 128-bit lanes; the permute puts the halves back in order.
            let packed = _mm256_packs_epi32(convert(i), convert(i + 8));
            let ordered = _mm256_permute4x64_epi64::<0b11_01_10_00>(packed);
            _mm256_storeu_si256(out.as_mut_ptr().add(i) as *mut __m256i, ordered);
            i += 16;
        }
        for (slot, &value) in out[i..row.len()].iter_mut().zip(&row[i..]) {
            *slot = (value * multiplier).round_ties_even().clamp(-127.0, 127.0) as i16;
        }
    }
}

/// `out[r][j] = Σₖ a[r][k] · b[column j][k]`, exactly, in i32. `a` is `rows × k` int8 values
/// held as i16; `b` holds one row of `k` int8 values per output (the weights, stored transposed);
/// `columns` picks which outputs (all `width` of them, in order, when empty).
fn matmul(
    a: &[i16],
    rows: usize,
    k: usize,
    b: &[i8],
    columns: &[u32],
    width: usize,
    out: &mut [i32],
) {
    #[cfg(target_arch = "x86_64")]
    if k % 16 == 0 && std::arch::is_x86_feature_detected!("avx2") {
        // Safety: the CPU supports AVX2 (checked), `k` is a multiple of 16, and every row and
        // column read is in bounds (checked by the slices' lengths below).
        assert!(a.len() >= rows * k && out.len() >= rows * width);
        assert!(
            columns
                .iter()
                .all(|&column| (column as usize + 1) * k <= b.len())
        );
        assert!(!columns.is_empty() || width * k <= b.len());
        unsafe { matmul_avx2(a, rows, k, b, columns, width, out) };
        return;
    }
    for row in 0..rows {
        let a_row = &a[row * k..(row + 1) * k];
        for j in 0..width {
            let column = if columns.is_empty() {
                j
            } else {
                columns[j] as usize
            };
            let b_row = &b[column * k..(column + 1) * k];
            out[row * width + j] = a_row
                .iter()
                .zip(b_row)
                .map(|(&x, &y)| i32::from(x) * i32::from(y))
                .sum();
        }
    }
}

/// The AVX2 kernel: two input rows by four outputs at a time, 16 values per step, with
/// `vpmaddwd` (exact: int8 × int8 products and their sums fit easily in i32).
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn matmul_avx2(
    a: &[i16],
    rows: usize,
    k: usize,
    b: &[i8],
    columns: &[u32],
    width: usize,
    out: &mut [i32],
) {
    use std::arch::x86_64::*;
    let column = |j: usize| -> usize {
        if columns.is_empty() {
            j
        } else {
            columns[j] as usize
        }
    };
    // Sums the eight lanes of each of four accumulators: [Σs0, Σs1, Σs2, Σs3].
    #[inline(always)]
    unsafe fn reduce(s0: __m256i, s1: __m256i, s2: __m256i, s3: __m256i) -> [i32; 4] {
        unsafe {
            let t = _mm256_hadd_epi32(_mm256_hadd_epi32(s0, s1), _mm256_hadd_epi32(s2, s3));
            let sum = _mm_add_epi32(_mm256_castsi256_si128(t), _mm256_extracti128_si256(t, 1));
            let mut lanes = [0i32; 4];
            _mm_storeu_si128(lanes.as_mut_ptr() as *mut __m128i, sum);
            lanes
        }
    }
    unsafe {
        let mut row = 0;
        while row < rows {
            let two = rows - row >= 2;
            let a0 = a.as_ptr().add(row * k);
            let a1 = if two {
                a.as_ptr().add((row + 1) * k)
            } else {
                a0
            };
            let mut j = 0;
            while j < width {
                let count = (width - j).min(4);
                // Fewer than four outputs left: repeat the last one and drop its duplicates.
                let w = |t: usize| b.as_ptr().add(column(j + t.min(count - 1)) * k);
                let (w0, w1, w2, w3) = (w(0), w(1), w(2), w(3));
                let zero = _mm256_setzero_si256();
                let (mut s00, mut s01, mut s02, mut s03) = (zero, zero, zero, zero);
                let (mut s10, mut s11, mut s12, mut s13) = (zero, zero, zero, zero);
                let mut i = 0;
                if !two {
                    // One row (decoding a word): don't compute it twice.
                    while i < k {
                        let x0 = _mm256_loadu_si256(a0.add(i) as *const __m256i);
                        let y0 = _mm256_cvtepi8_epi16(_mm_loadu_si128(w0.add(i) as *const __m128i));
                        let y1 = _mm256_cvtepi8_epi16(_mm_loadu_si128(w1.add(i) as *const __m128i));
                        let y2 = _mm256_cvtepi8_epi16(_mm_loadu_si128(w2.add(i) as *const __m128i));
                        let y3 = _mm256_cvtepi8_epi16(_mm_loadu_si128(w3.add(i) as *const __m128i));
                        s00 = _mm256_add_epi32(s00, _mm256_madd_epi16(x0, y0));
                        s01 = _mm256_add_epi32(s01, _mm256_madd_epi16(x0, y1));
                        s02 = _mm256_add_epi32(s02, _mm256_madd_epi16(x0, y2));
                        s03 = _mm256_add_epi32(s03, _mm256_madd_epi16(x0, y3));
                        i += 16;
                    }
                }
                while i < k {
                    let x0 = _mm256_loadu_si256(a0.add(i) as *const __m256i);
                    let x1 = _mm256_loadu_si256(a1.add(i) as *const __m256i);
                    let y0 = _mm256_cvtepi8_epi16(_mm_loadu_si128(w0.add(i) as *const __m128i));
                    s00 = _mm256_add_epi32(s00, _mm256_madd_epi16(x0, y0));
                    s10 = _mm256_add_epi32(s10, _mm256_madd_epi16(x1, y0));
                    let y1 = _mm256_cvtepi8_epi16(_mm_loadu_si128(w1.add(i) as *const __m128i));
                    s01 = _mm256_add_epi32(s01, _mm256_madd_epi16(x0, y1));
                    s11 = _mm256_add_epi32(s11, _mm256_madd_epi16(x1, y1));
                    let y2 = _mm256_cvtepi8_epi16(_mm_loadu_si128(w2.add(i) as *const __m128i));
                    s02 = _mm256_add_epi32(s02, _mm256_madd_epi16(x0, y2));
                    s12 = _mm256_add_epi32(s12, _mm256_madd_epi16(x1, y2));
                    let y3 = _mm256_cvtepi8_epi16(_mm_loadu_si128(w3.add(i) as *const __m128i));
                    s03 = _mm256_add_epi32(s03, _mm256_madd_epi16(x0, y3));
                    s13 = _mm256_add_epi32(s13, _mm256_madd_epi16(x1, y3));
                    i += 16;
                }
                let first = reduce(s00, s01, s02, s03);
                out[row * width + j..row * width + j + count].copy_from_slice(&first[..count]);
                if two {
                    let second = reduce(s10, s11, s12, s13);
                    out[(row + 1) * width + j..(row + 1) * width + j + count]
                        .copy_from_slice(&second[..count]);
                }
                j += 4;
            }
            row += 2;
        }
    }
}

struct LayerNorm {
    scale: Vec<f32>,
    bias: Vec<f32>,
}

impl LayerNorm {
    fn load(items: &HashMap<String, Item>, prefix: &str) -> Result<LayerNorm> {
        Ok(LayerNorm {
            scale: item(items, &format!("{prefix}_ln_scale"))?.floats()?,
            bias: item(items, &format!("{prefix}_ln_bias"))?.floats()?,
        })
    }

    /// `x = norm(x + residual)`, row by row: Marian's "dan" post-processing (dropout is a no-op
    /// when translating).
    fn add_and_norm(&self, x: &mut [f32], residual: &[f32]) {
        let dim = self.scale.len();
        for (row, residual) in x.chunks_exact_mut(dim).zip(residual.chunks_exact(dim)) {
            for (value, &extra) in row.iter_mut().zip(residual) {
                *value += extra;
            }
            let mean = row.iter().sum::<f32>() / dim as f32;
            let variance = row
                .iter()
                .map(|value| (value - mean) * (value - mean))
                .sum::<f32>()
                / dim as f32;
            let inverse = 1.0 / (variance + 1e-9).sqrt();
            for ((value, &scale), &bias) in row.iter_mut().zip(&self.scale).zip(&self.bias) {
                *value = scale * (*value - mean) * inverse + bias;
            }
        }
    }
}

struct Attention {
    query: Linear,
    key: Linear,
    value: Linear,
    output: Linear,
    norm: LayerNorm,
}

impl Attention {
    fn load(items: &HashMap<String, Item>, prefix: &str) -> Result<Attention> {
        let linear = |name: &str| {
            Linear::load(
                items,
                &format!("{prefix}_W{name}"),
                Some(&format!("{prefix}_b{name}")),
            )
        };
        Ok(Attention {
            query: linear("q")?,
            key: linear("k")?,
            value: linear("v")?,
            output: linear("o")?,
            norm: LayerNorm::load(items, &format!("{prefix}_Wo"))?,
        })
    }
}

struct FeedForward {
    first: Linear,
    second: Linear,
    norm: LayerNorm,
    swish: bool,
}

impl FeedForward {
    fn load(items: &HashMap<String, Item>, prefix: &str, swish: bool) -> Result<FeedForward> {
        Ok(FeedForward {
            first: Linear::load(
                items,
                &format!("{prefix}_W1"),
                Some(&format!("{prefix}_b1")),
            )?,
            second: Linear::load(
                items,
                &format!("{prefix}_W2"),
                Some(&format!("{prefix}_b2")),
            )?,
            norm: LayerNorm::load(items, &format!("{prefix}_ffn"))?,
            swish,
        })
    }

    fn apply(&self, x: &mut [f32], rows: usize) {
        let mut hidden = self.first.forward(x, rows);
        for value in hidden.iter_mut() {
            *value = if self.swish {
                *value / (1.0 + (-*value).exp())
            } else {
                value.max(0.0)
            };
        }
        let out = self.second.forward(&hidden, rows);
        // x = norm(out + x)
        let residual = x.to_vec();
        x.copy_from_slice(&out);
        self.norm.add_and_norm(x, &residual);
    }
}

struct EncoderLayer {
    attention: Attention,
    feed_forward: FeedForward,
}

struct DecoderLayer {
    rnn_input: Linear,
    rnn_forget: Linear,
    rnn_norm: LayerNorm,
    context: Attention,
    feed_forward: FeedForward,
}

pub(crate) struct Network {
    dim: usize,
    heads: usize,
    /// Target embeddings, also used to score each next word.
    embeddings: Linear,
    /// Source embeddings, when the model has its own for the source language.
    source_embeddings: Option<Linear>,
    encoder: Vec<EncoderLayer>,
    decoder: Vec<DecoderLayer>,
    vocab_size: usize,
}

/// Multi-head scaled dot-product attention of `queries` over `keys`/`values` (already projected).
/// With `head0`, the first head's weights for the last query are copied there: the models were
/// trained to align words with that head of the last decoder layer.
fn attend(
    queries: &[f32],
    keys: &[f32],
    values: &[f32],
    dim: usize,
    heads: usize,
    mut head0: Option<&mut Vec<f32>>,
) -> Vec<f32> {
    let query_rows = queries.len() / dim;
    let key_rows = keys.len() / dim;
    let depth = dim / heads;
    let scale = 1.0 / (depth as f32).sqrt();
    let mut out = vec![0f32; queries.len()];
    let mut weights = vec![0f32; key_rows];
    for head in 0..heads {
        let span = head * depth..(head + 1) * depth;
        for row in 0..query_rows {
            let query = &queries[row * dim..][span.clone()];
            let mut largest = f32::NEG_INFINITY;
            for (key_row, weight) in weights.iter_mut().enumerate() {
                let key = &keys[key_row * dim..][span.clone()];
                *weight = query.iter().zip(key).map(|(a, b)| a * b).sum::<f32>() * scale;
                largest = largest.max(*weight);
            }
            let mut total = 0.0;
            for weight in weights.iter_mut() {
                *weight = (*weight - largest).exp();
                total += *weight;
            }
            if head == 0 && row + 1 == query_rows {
                if let Some(copy) = head0.as_deref_mut() {
                    copy.clear();
                    copy.extend(weights.iter().map(|weight| weight / total));
                }
            }
            let target = &mut out[row * dim..][span.clone()];
            for (key_row, weight) in weights.iter().enumerate() {
                let value = &values[key_row * dim..][span.clone()];
                let weight = weight / total;
                for (slot, &v) in target.iter_mut().zip(value) {
                    *slot += weight * v;
                }
            }
        }
    }
    out
}

/// Rows the encoder processes at once (see `translate_batch`).
const ENCODE_ROWS: usize = 128;

fn add_positions(x: &mut [f32], dim: usize, start: usize) {
    let timescales = dim / 2;
    for (offset, row) in x.chunks_exact_mut(dim).enumerate() {
        let position = (start + offset) as f32;
        for (index, value) in row.iter_mut().enumerate() {
            let frequency = 1e-4f32.powf((index % timescales) as f32 / (timescales as f32 - 1.0));
            let phase = (index / timescales) as f32 * FRAC_PI_2;
            *value += (position * frequency + phase).sin();
        }
    }
}

impl Network {
    pub(crate) fn load(bytes: &[u8]) -> Result<Network> {
        let items = crate::model::read(bytes)?;
        let settings = config(&items);
        let setting = |key: &str| settings.get(key).map(String::as_str).unwrap_or("");
        let expect = |key: &str, wanted: &[&str]| -> Result<()> {
            let value = setting(key);
            if wanted.contains(&value) {
                Ok(())
            } else {
                Err(invalid(format!("unsupported model: {key} is \"{value}\"")))
            }
        };
        expect("type", &["transformer"])?;
        expect("transformer-decoder-autoreg", &["rnn"])?;
        expect("dec-cell", &["ssru"])?;
        expect("transformer-postprocess", &["dan"])?;
        expect("transformer-preprocess", &[""])?;
        expect("transformer-postprocess-emb", &["d", ""])?;
        expect("transformer-postprocess-top", &[""])?;
        expect("transformer-ffn-depth", &["2"])?;
        // One embedding table for both languages, or one each with the target's also scoring
        // the output.
        let tied_all = setting("tied-embeddings-all") == "true";
        if !tied_all {
            expect("tied-embeddings", &["true"])?;
        }
        let number = |key: &str| -> Result<usize> {
            setting(key)
                .parse()
                .map_err(|_| invalid(format!("model setting {key} is missing")))
        };
        let dim = number("dim-emb")?;
        let heads = number("transformer-heads")?;
        if heads == 0 || dim % heads != 0 || dim < 4 {
            return Err(invalid("bad model dimensions"));
        }
        let swish = setting("transformer-ffn-activation") == "swish";
        let encoder = (1..=number("enc-depth")?)
            .map(|layer| {
                let prefix = format!("encoder_l{layer}");
                Ok(EncoderLayer {
                    attention: Attention::load(&items, &format!("{prefix}_self"))?,
                    feed_forward: FeedForward::load(&items, &format!("{prefix}_ffn"), swish)?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let decoder = (1..=number("dec-depth")?)
            .map(|layer| {
                let prefix = format!("decoder_l{layer}");
                Ok(DecoderLayer {
                    rnn_input: Linear::load(&items, &format!("{prefix}_rnn_W"), None)?,
                    rnn_forget: Linear::load(
                        &items,
                        &format!("{prefix}_rnn_Wf"),
                        Some(&format!("{prefix}_rnn_bf")),
                    )?,
                    rnn_norm: LayerNorm::load(&items, &format!("{prefix}_rnn_ffn"))?,
                    context: Attention::load(&items, &format!("{prefix}_context"))?,
                    feed_forward: FeedForward::load(&items, &format!("{prefix}_ffn"), swish)?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let (embeddings, source_embeddings) = if tied_all {
            (
                Linear::load(&items, "Wemb", Some("decoder_ff_logit_out_b"))?,
                None,
            )
        } else {
            (
                Linear::load(&items, "decoder_Wemb", Some("decoder_ff_logit_out_b"))?,
                Some(Linear::load(&items, "encoder_Wemb", None)?),
            )
        };
        if embeddings.inputs != dim
            || source_embeddings
                .as_ref()
                .is_some_and(|table| table.inputs != dim)
        {
            return Err(invalid("embedding size doesn't match the model"));
        }
        let vocab_size = embeddings.outputs;
        Ok(Network {
            dim,
            heads,
            embeddings,
            source_embeddings,
            encoder,
            decoder,
            vocab_size,
        })
    }

    pub(crate) fn vocab_size(&self) -> usize {
        self.vocab_size
    }

    fn embed(&self, table: &Linear, word: u32, out: &mut [f32]) {
        let scale = (self.dim as f32).sqrt();
        if word as usize >= table.outputs {
            out.fill(0.0);
            return;
        }
        for (slot, value) in out.iter_mut().zip(table.row(word as usize)) {
            *slot = value * scale;
        }
    }

    /// Encodes several sentences at once: the layers' weights are applied to all their rows
    /// together (each weight read once for the batch), attention stays within each sentence.
    /// `starts` holds each sentence's first row, plus the total at the end.
    fn encode_batch(&self, sources: &[&[u32]], starts: &[usize]) -> Vec<f32> {
        let dim = self.dim;
        let rows = *starts.last().unwrap_or(&0);
        let mut x = vec![0f32; rows * dim];
        let table = self.source_embeddings.as_ref().unwrap_or(&self.embeddings);
        for (sentence, source) in sources.iter().enumerate() {
            for (offset, &word) in source.iter().enumerate() {
                let row = starts[sentence] + offset;
                self.embed(table, word, &mut x[row * dim..(row + 1) * dim]);
            }
            add_positions(
                &mut x[starts[sentence] * dim..starts[sentence + 1] * dim],
                dim,
                0,
            );
        }
        for layer in &self.encoder {
            let attention = &layer.attention;
            let queries = attention.query.forward(&x, rows);
            let keys = attention.key.forward(&x, rows);
            let values = attention.value.forward(&x, rows);
            let mut attended = vec![0f32; rows * dim];
            for sentence in 0..sources.len() {
                let span = starts[sentence] * dim..starts[sentence + 1] * dim;
                let result = attend(
                    &queries[span.clone()],
                    &keys[span.clone()],
                    &values[span.clone()],
                    dim,
                    self.heads,
                    None,
                );
                attended[span].copy_from_slice(&result);
            }
            let mut out = attention.output.forward(&attended, rows);
            attention.norm.add_and_norm(&mut out, &x);
            x = out;
            layer.feed_forward.apply(&mut x, rows);
        }
        x
    }

    /// Greedy decoding of a batch of sentences together: each step, the next piece of every
    /// unfinished sentence, until end of sentence (or its `max_length`). Each piece comes with
    /// the index of the source piece it's aligned to. Every sentence gets exactly the result
    /// it would get on its own; batching only shares the reading of the weights.
    pub(crate) fn translate_batch(
        &self,
        sources: &[&[u32]],
        candidates: &[&[u32]],
        eos: u32,
        unknown: u32,
        max_lengths: &[usize],
    ) -> Vec<Vec<(u32, usize)>> {
        let dim = self.dim;
        let count = sources.len();
        let mut starts = Vec::with_capacity(count + 1);
        starts.push(0);
        for source in sources {
            starts.push(starts.last().unwrap() + source.len());
        }
        // The encoder works on groups of up to ENCODE_ROWS rows: big enough to share the weights,
        // small enough to stay in the CPU's cache.
        let started = profile::start();
        let mut context = Vec::with_capacity(starts[count] * dim);
        let mut group = 0;
        while group < count {
            let mut end = group + 1;
            while end < count && starts[end + 1] - starts[group] <= ENCODE_ROWS {
                end += 1;
            }
            let local: Vec<usize> = starts[group..=end]
                .iter()
                .map(|start| start - starts[group])
                .collect();
            context.extend(self.encode_batch(&sources[group..end], &local));
            group = end;
        }
        profile::stop(0, started);
        let rows = *starts.last().unwrap();
        // Keys and values of the source, per decoder layer (all sentences, sliced by `starts`).
        let projected: Vec<(Vec<f32>, Vec<f32>)> = self
            .decoder
            .iter()
            .map(|layer| {
                (
                    layer.context.key.forward(&context, rows),
                    layer.context.value.forward(&context, rows),
                )
            })
            .collect();

        let mut outputs: Vec<Vec<(u32, usize)>> = vec![Vec::new(); count];
        // The sentences still being decoded, and their state (rows in the same order).
        let mut active: Vec<usize> = (0..count).filter(|&s| max_lengths[s] > 0).collect();
        let mut previous: Vec<Option<u32>> = vec![None; count];
        let mut cells: Vec<Vec<f32>> = vec![vec![0f32; count * dim]; self.decoder.len()];
        let mut alignments: Vec<Vec<f32>> = vec![Vec::new(); count];
        let last_layer = self.decoder.len().saturating_sub(1);
        let mut step = 0;
        while !active.is_empty() {
            let batch = active.len();
            let mut x = vec![0f32; batch * dim];
            for (row, &sentence) in active.iter().enumerate() {
                let slot = &mut x[row * dim..(row + 1) * dim];
                if let Some(word) = previous[sentence] {
                    self.embed(&self.embeddings, word, slot);
                }
                add_positions(slot, dim, step);
            }
            for (index, (layer, (keys, values))) in self.decoder.iter().zip(&projected).enumerate()
            {
                // SSRU: c = σ(f)·c + (1 − σ(f))·xW, output relu(c), then x = norm(output + x).
                let started = profile::start();
                let input = layer.rnn_input.forward(&x, batch);
                let forget = layer.rnn_forget.forward(&x, batch);
                let mut out = vec![0f32; batch * dim];
                for (row, &sentence) in active.iter().enumerate() {
                    let cell = &mut cells[index][sentence * dim..(sentence + 1) * dim];
                    for i in 0..dim {
                        let at = row * dim + i;
                        let gate = 1.0 / (1.0 + (-forget[at]).exp());
                        cell[i] = gate * cell[i] + (1.0 - gate) * input[at];
                        out[at] = cell[i].max(0.0);
                    }
                }
                layer.rnn_norm.add_and_norm(&mut out, &x);
                x = out;
                profile::stop(1, started);

                let started = profile::start();
                let queries = layer.context.query.forward(&x, batch);
                let mut attended = vec![0f32; batch * dim];
                for (row, &sentence) in active.iter().enumerate() {
                    let span = starts[sentence] * dim..starts[sentence + 1] * dim;
                    let capture = (index == last_layer).then_some(&mut alignments[sentence]);
                    let result = attend(
                        &queries[row * dim..(row + 1) * dim],
                        &keys[span.clone()],
                        &values[span],
                        dim,
                        self.heads,
                        capture,
                    );
                    attended[row * dim..(row + 1) * dim].copy_from_slice(&result);
                }
                let mut out = layer.context.output.forward(&attended, batch);
                layer.context.norm.add_and_norm(&mut out, &x);
                x = out;
                profile::stop(2, started);

                let started = profile::start();
                layer.feed_forward.apply(&mut x, batch);
                profile::stop(3, started);
            }
            // Each sentence scores its own candidates (its shortlist).
            let started = profile::start();
            let mut finished = Vec::new();
            for (row, &sentence) in active.iter().enumerate() {
                let words = candidates[sentence];
                let scores =
                    self.embeddings
                        .forward_columns(&x[row * dim..(row + 1) * dim], 1, words);
                let mut best = (f32::NEG_INFINITY, eos);
                for (index, &score) in scores.iter().enumerate() {
                    let word = if words.is_empty() {
                        index as u32
                    } else {
                        words[index]
                    };
                    if word != unknown && score > best.0 {
                        best = (score, word);
                    }
                }
                if best.1 == eos {
                    finished.push(sentence);
                    continue;
                }
                let aligned = alignments[sentence]
                    .iter()
                    .enumerate()
                    .fold((0, f32::NEG_INFINITY), |best, (index, &weight)| {
                        if weight > best.1 {
                            (index, weight)
                        } else {
                            best
                        }
                    })
                    .0;
                outputs[sentence].push((best.1, aligned));
                previous[sentence] = Some(best.1);
                if outputs[sentence].len() >= max_lengths[sentence] {
                    finished.push(sentence);
                }
            }
            profile::stop(4, started);
            active.retain(|sentence| !finished.contains(sentence));
            step += 1;
        }
        outputs
    }
}
