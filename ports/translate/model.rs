/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Reads Marian's binary model format (`*.bin`), as used by Firefox Translations' models.
//!
//! Layout: `u64 version, u64 count`, then `count` headers of four `u64` (name length, type,
//! shape length, data length), the names (NUL-terminated), the shapes (`i32`s), a `u64` padding
//! length and that much padding, then every item's data in order.

use std::collections::HashMap;
use std::io::{Error, ErrorKind, Result};

/// Marian's type tags (`marian::Type`).
pub(crate) const TYPE_FLOAT32: u64 = 0x0404;
pub(crate) const TYPE_INTGEMM8: u64 = 0x4101;

pub(crate) struct Item {
    pub ty: u64,
    pub shape: Vec<usize>,
    pub data: Vec<u8>,
}

impl Item {
    pub fn elements(&self) -> usize {
        self.shape.iter().product()
    }

    pub fn floats(&self) -> Result<Vec<f32>> {
        if self.ty != TYPE_FLOAT32 {
            return Err(invalid(format!(
                "expected float32 data, found type {:#x}",
                self.ty
            )));
        }
        let count = self.elements();
        if self.data.len() < count * 4 {
            return Err(invalid("float item shorter than its shape"));
        }
        Ok(self.data[..count * 4]
            .chunks_exact(4)
            .map(|bytes| f32::from_le_bytes(bytes.try_into().unwrap()))
            .collect())
    }

    pub fn scalar(&self) -> Result<f32> {
        // QuantMultA items are stored as float32 or, for Wemb, with the int8 tag; either way
        // the first four bytes are the float.
        if self.data.len() < 4 {
            return Err(invalid("scalar item is empty"));
        }
        Ok(f32::from_le_bytes(self.data[..4].try_into().unwrap()))
    }

    /// An `intgemm8` matrix: int8 values stored transposed (`[columns][rows]` of `shape`), followed
    /// by the float the values were multiplied by when quantized.
    pub fn int8(&self) -> Result<(Vec<i8>, f32)> {
        if self.ty != TYPE_INTGEMM8 {
            return Err(invalid(format!(
                "expected intgemm8 data, found type {:#x}",
                self.ty
            )));
        }
        let count = self.elements();
        if self.data.len() < count + 4 {
            return Err(invalid("int8 item shorter than its shape"));
        }
        let values = self.data[..count].iter().map(|&byte| byte as i8).collect();
        let multiplier = f32::from_le_bytes(self.data[count..count + 4].try_into().unwrap());
        Ok((values, multiplier))
    }
}

pub(crate) fn invalid(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::InvalidData, message.into())
}

fn read_u64(bytes: &[u8], at: &mut usize) -> Result<u64> {
    let slice = bytes
        .get(*at..*at + 8)
        .ok_or_else(|| invalid("model file is truncated"))?;
    *at += 8;
    Ok(u64::from_le_bytes(slice.try_into().unwrap()))
}

pub(crate) fn read(bytes: &[u8]) -> Result<HashMap<String, Item>> {
    let mut at = 0;
    let version = read_u64(bytes, &mut at)?;
    if version != 1 {
        return Err(invalid(format!(
            "unsupported Marian model version {version}"
        )));
    }
    let count = read_u64(bytes, &mut at)? as usize;
    if count > 100_000 {
        return Err(invalid("implausible item count"));
    }
    let mut headers = Vec::with_capacity(count);
    for _ in 0..count {
        let name_length = read_u64(bytes, &mut at)? as usize;
        let ty = read_u64(bytes, &mut at)?;
        let shape_length = read_u64(bytes, &mut at)? as usize;
        let data_length = read_u64(bytes, &mut at)? as usize;
        headers.push((name_length, ty, shape_length, data_length));
    }
    let mut names = Vec::with_capacity(count);
    for &(name_length, ..) in &headers {
        let raw = bytes
            .get(at..at + name_length)
            .ok_or_else(|| invalid("model file is truncated"))?;
        at += name_length;
        let end = raw.iter().position(|&byte| byte == 0).unwrap_or(raw.len());
        names.push(String::from_utf8_lossy(&raw[..end]).into_owned());
    }
    let mut shapes = Vec::with_capacity(count);
    for &(_, _, shape_length, _) in &headers {
        let raw = bytes
            .get(at..at + shape_length * 4)
            .ok_or_else(|| invalid("model file is truncated"))?;
        at += shape_length * 4;
        shapes.push(
            raw.chunks_exact(4)
                .map(|value| i32::from_le_bytes(value.try_into().unwrap()).max(0) as usize)
                .collect::<Vec<_>>(),
        );
    }
    let padding = read_u64(bytes, &mut at)? as usize;
    at += padding;
    let mut items = HashMap::with_capacity(count);
    for ((name, shape), (_, ty, _, data_length)) in names.into_iter().zip(shapes).zip(headers) {
        let data = bytes
            .get(at..at + data_length)
            .ok_or_else(|| invalid("model file is truncated"))?
            .to_vec();
        at += data_length;
        items.insert(name, Item { ty, shape, data });
    }
    Ok(items)
}

/// The model's `special:model.yml`: flat `key: value` lines are all that's needed.
pub(crate) fn config(items: &HashMap<String, Item>) -> HashMap<String, String> {
    let Some(item) = items.get("special:model.yml") else {
        return HashMap::new();
    };
    String::from_utf8_lossy(&item.data)
        .lines()
        .filter_map(|line| {
            let (key, value) = line.split_once(':')?;
            let value = value.trim();
            // YAML strings may be quoted: `transformer-preprocess: ""`.
            let value = value
                .strip_prefix('"')
                .and_then(|rest| rest.strip_suffix('"'))
                .unwrap_or(value);
            (!line.starts_with(' ')).then(|| (key.trim().to_owned(), value.to_owned()))
        })
        .collect()
}
