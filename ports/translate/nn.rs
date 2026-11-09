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

    fn quantize(&self, row: &[f32], out: &mut [i8]) -> f32 {
        let multiplier = if self.input_multiplier > 0.0 {
            self.input_multiplier
        } else {
            let largest = row.iter().fold(0f32, |max, value| max.max(value.abs()));
            if largest > 0.0 { 127.0 / largest } else { 1.0 }
        };
        for (slot, &value) in out.iter_mut().zip(row) {
            *slot = (value * multiplier).round_ties_even().clamp(-127.0, 127.0) as i8;
        }
        multiplier
    }

    /// `rows` inputs, one after another, to `rows` outputs.
    pub(crate) fn forward(&self, input: &[f32], rows: usize) -> Vec<f32> {
        let all: Vec<u32> = Vec::new();
        self.forward_columns(input, rows, &all)
    }

    /// Like `forward`, computing only the given outputs (all of them when `columns` is empty).
    pub(crate) fn forward_columns(&self, input: &[f32], rows: usize, columns: &[u32]) -> Vec<f32> {
        let width = if columns.is_empty() {
            self.outputs
        } else {
            columns.len()
        };
        let mut out = vec![0f32; rows * width];
        let mut quantized = vec![0i8; self.inputs];
        for row in 0..rows {
            let input_multiplier = self.quantize(
                &input[row * self.inputs..(row + 1) * self.inputs],
                &mut quantized,
            );
            let scale = 1.0 / (input_multiplier * self.weight_multiplier);
            let out_row = &mut out[row * width..(row + 1) * width];
            products(&quantized, &self.weights, self.inputs, columns, out_row);
            for (index, value) in out_row.iter_mut().enumerate() {
                let column = if columns.is_empty() {
                    index
                } else {
                    columns[index] as usize
                };
                *value *= scale;
                if let Some(bias) = &self.bias {
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

#[inline(always)]
fn dot(a: &[i8], b: &[i8]) -> i32 {
    let mut lanes = [0i32; 32];
    let mut a_chunks = a.chunks_exact(32);
    let mut b_chunks = b.chunks_exact(32);
    for (x, y) in (&mut a_chunks).zip(&mut b_chunks) {
        for lane in 0..32 {
            lanes[lane] += i32::from(x[lane]) * i32::from(y[lane]);
        }
    }
    let mut total: i32 = lanes.iter().sum();
    for (&x, &y) in a_chunks.remainder().iter().zip(b_chunks.remainder()) {
        total += i32::from(x) * i32::from(y);
    }
    total
}

#[inline(always)]
fn products_generic(input: &[i8], weights: &[i8], inputs: usize, columns: &[u32], out: &mut [f32]) {
    if columns.is_empty() {
        for (column, value) in out.iter_mut().enumerate() {
            *value = dot(input, &weights[column * inputs..(column + 1) * inputs]) as f32;
        }
    } else {
        for (value, &column) in out.iter_mut().zip(columns) {
            let column = column as usize;
            *value = dot(input, &weights[column * inputs..(column + 1) * inputs]) as f32;
        }
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn products_avx2(
    input: &[i8],
    weights: &[i8],
    inputs: usize,
    columns: &[u32],
    out: &mut [f32],
) {
    products_generic(input, weights, inputs, columns, out)
}

fn products(input: &[i8], weights: &[i8], inputs: usize, columns: &[u32], out: &mut [f32]) {
    #[cfg(target_arch = "x86_64")]
    if std::arch::is_x86_feature_detected!("avx2") {
        // Safety: the CPU supports AVX2, checked just above.
        unsafe { products_avx2(input, weights, inputs, columns, out) };
        return;
    }
    products_generic(input, weights, inputs, columns, out)
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

    fn encode(&self, source: &[u32]) -> Vec<f32> {
        let dim = self.dim;
        let rows = source.len();
        let mut x = vec![0f32; rows * dim];
        let table = self.source_embeddings.as_ref().unwrap_or(&self.embeddings);
        for (row, &word) in source.iter().enumerate() {
            self.embed(table, word, &mut x[row * dim..(row + 1) * dim]);
        }
        add_positions(&mut x, dim, 0);
        for layer in &self.encoder {
            let attention = &layer.attention;
            let queries = attention.query.forward(&x, rows);
            let keys = attention.key.forward(&x, rows);
            let values = attention.value.forward(&x, rows);
            let attended = attend(&queries, &keys, &values, dim, self.heads, None);
            let mut out = attention.output.forward(&attended, rows);
            attention.norm.add_and_norm(&mut out, &x);
            x = out;
            layer.feed_forward.apply(&mut x, rows);
        }
        x
    }

    /// Greedy decoding: the most likely next piece each step, until end of sentence. Each piece
    /// comes with the index of the source piece it's aligned to.
    pub(crate) fn translate(
        &self,
        source: &[u32],
        candidates: &[u32],
        eos: u32,
        unknown: u32,
        max_length: usize,
    ) -> Vec<(u32, usize)> {
        let dim = self.dim;
        let context = self.encode(source);
        let rows = source.len();
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
        let mut cells = vec![vec![0f32; dim]; self.decoder.len()];
        let mut output = Vec::new();
        let mut previous: Option<u32> = None;
        let mut x = vec![0f32; dim];
        let mut alignment = Vec::with_capacity(rows);
        let last_layer = self.decoder.len().saturating_sub(1);
        for step in 0..max_length {
            match previous {
                Some(word) => self.embed(&self.embeddings, word, &mut x),
                None => x.fill(0.0),
            }
            add_positions(&mut x, dim, step);
            for (index, ((layer, cell), (keys, values))) in self
                .decoder
                .iter()
                .zip(&mut cells)
                .zip(&projected)
                .enumerate()
            {
                // SSRU: c = σ(f)·c + (1 − σ(f))·xW, output relu(c), then x = norm(output + x).
                let input = layer.rnn_input.forward(&x, 1);
                let forget = layer.rnn_forget.forward(&x, 1);
                let mut out = vec![0f32; dim];
                for i in 0..dim {
                    let gate = 1.0 / (1.0 + (-forget[i]).exp());
                    cell[i] = gate * cell[i] + (1.0 - gate) * input[i];
                    out[i] = cell[i].max(0.0);
                }
                layer.rnn_norm.add_and_norm(&mut out, &x);
                x = out;

                let queries = layer.context.query.forward(&x, 1);
                let capture = (index == last_layer).then_some(&mut alignment);
                let attended = attend(&queries, keys, values, dim, self.heads, capture);
                let mut out = layer.context.output.forward(&attended, 1);
                layer.context.norm.add_and_norm(&mut out, &x);
                x = out;

                layer.feed_forward.apply(&mut x, 1);
            }
            let scores = self.embeddings.forward_columns(&x, 1, candidates);
            let mut best = (f32::NEG_INFINITY, eos);
            for (index, &score) in scores.iter().enumerate() {
                let word = if candidates.is_empty() {
                    index as u32
                } else {
                    candidates[index]
                };
                if word != unknown && score > best.0 {
                    best = (score, word);
                }
            }
            if best.1 == eos {
                break;
            }
            let aligned = alignment
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
            output.push((best.1, aligned));
            previous = Some(best.1);
        }
        output
    }
}
