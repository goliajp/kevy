//! How one index entry becomes bytes: an order key `E = V ‖ H` that
//! compares with `memcmp` exactly as `(value, key)` compares, and a
//! payload with the row's stored `VALUES` columns.
//!
//! * `V` is the value: an `i64` or `f64` as its 8 order bytes, a
//!   composite value as its own (already self-delimiting) encoding, any
//!   other string framed — `0x00` escaped as `0x00 0xFF`, then `0x00
//!   0x00` — so a shorter string sorts before its extensions whatever key
//!   follows it.
//! * `H` is the key with the segment's prefix taken off. While every
//!   suffix is decimal digits it is packed two digits a byte (digit `d`
//!   as nibble `d + 1`, an odd count padded with a zero nibble), which
//!   keeps byte order equal to the digits' own order; the first other
//!   suffix switches the segment to plain bytes for good.
//!
//! A stored column is a varint tag and its body: `0` absent, an odd tag
//! `2n + 1` for a string of `n` digits packed as above, an even tag
//! `2n + 2` for `n` plain bytes. What comes back is exactly what went in.

use crate::catalog::ValType;
use crate::value::IndexValue;
use kevy_text::SortOrder;

/// How a segment's values are laid out in `V`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Form {
    /// No value seen yet: the first one decides.
    Unset,
    I64,
    F64,
    /// A plain string, framed.
    Str,
    /// A composite encoding over these columns, stored as it is.
    Composite(Vec<(ValType, SortOrder)>),
}

impl Form {
    /// The form a value takes in a segment that has not fixed one.
    pub(crate) fn of(v: &IndexValue) -> Form {
        match v {
            IndexValue::I64(_) => Form::I64,
            IndexValue::F64(_) => Form::F64,
            IndexValue::Str(_) => Form::Str,
        }
    }

    /// Whether `v` is of this form's value type.
    pub(crate) fn admits(&self, v: &IndexValue) -> bool {
        matches!(
            (self, v),
            (Form::I64, IndexValue::I64(_))
                | (Form::F64, IndexValue::F64(_))
                | (Form::Str | Form::Composite(_), IndexValue::Str(_))
        )
    }

    /// The rank `IndexValue`'s cross-type order gives this form's type.
    pub(crate) fn rank(&self) -> u8 {
        match self {
            Form::I64 => 0,
            Form::F64 => 1,
            Form::Str | Form::Composite(_) | Form::Unset => 2,
        }
    }
}

/// The rank `IndexValue`'s cross-type order gives `v`.
pub(crate) fn rank_of(v: &IndexValue) -> u8 {
    match v {
        IndexValue::I64(_) => 0,
        IndexValue::F64(_) => 1,
        IndexValue::Str(_) => 2,
    }
}

/// A segment's encoding: its value form, the key prefix every row shares,
/// whether key suffixes are digit-packed, and how many stored columns.
#[derive(Debug, Clone)]
pub(crate) struct Codec {
    pub(crate) form: Form,
    pub(crate) prefix: Box<[u8]>,
    pub(crate) digits: bool,
    pub(crate) arity: usize,
}

impl Codec {
    pub(crate) fn new(form: Form, prefix: &[u8], arity: usize) -> Codec {
        Codec { form, prefix: prefix.into(), digits: true, arity }
    }

    /// Whether `key` can be encoded without changing the segment's mode.
    pub(crate) fn fits_key(&self, key: &[u8]) -> bool {
        key.starts_with(&self.prefix)
            && (!self.digits || key[self.prefix.len()..].iter().all(u8::is_ascii_digit))
    }

    /// The codec after `key` arrived: plain suffixes if it is not
    /// digits, no prefix if it lacks the prefix.
    pub(crate) fn widened_for(&self, key: &[u8]) -> Codec {
        let mut c = self.clone();
        if !key.starts_with(&self.prefix) {
            c.prefix = Box::default();
        }
        if !key[c.prefix.len()..].iter().all(u8::is_ascii_digit) {
            c.digits = false;
        }
        c
    }

    /// Append `V` for `v` (which [`Form::admits`]).
    pub(crate) fn put_value(&self, v: &IndexValue, out: &mut Vec<u8>) {
        match v {
            IndexValue::I64(i) => out.extend_from_slice(&((*i as u64) ^ (1 << 63)).to_be_bytes()),
            IndexValue::F64(f) => out.extend_from_slice(&f64_order(*f).to_be_bytes()),
            IndexValue::Str(s) if matches!(self.form, Form::Composite(_)) => {
                out.extend_from_slice(s)
            }
            IndexValue::Str(s) => frame(s, out),
        }
    }

    /// Append `H` for `key` (which [`Codec::fits_key`]).
    pub(crate) fn put_handle(&self, key: &[u8], out: &mut Vec<u8>) {
        let suffix = &key[self.prefix.len()..];
        if self.digits {
            pack_digits(suffix, out);
        } else {
            out.extend_from_slice(suffix);
        }
    }

    /// `E` for `(v, key)`.
    pub(crate) fn put_entry(&self, v: &IndexValue, key: &[u8], out: &mut Vec<u8>) {
        self.put_value(v, out);
        self.put_handle(key, out);
    }

    /// Length of the `V` part at the start of `e`.
    pub(crate) fn value_len(&self, e: &[u8]) -> usize {
        match &self.form {
            Form::I64 | Form::F64 => 8,
            Form::Str | Form::Unset => framed_len(e, false),
            Form::Composite(cols) => {
                cols.iter().fold(0, |at, (ty, order)| at + component_len(&e[at..], *ty, *order))
            }
        }
    }

    /// The value `V` holds.
    pub(crate) fn value(&self, v: &[u8]) -> IndexValue {
        match &self.form {
            Form::I64 => IndexValue::I64((be8(v) ^ (1 << 63)) as i64),
            Form::F64 => IndexValue::F64(f64_from_order(be8(v))),
            Form::Composite(_) => IndexValue::Str(v.to_vec()),
            Form::Str | Form::Unset => {
                let mut s = Vec::with_capacity(v.len());
                unframe_into(v, &mut s);
                IndexValue::Str(s)
            }
        }
    }

    /// Overwrite `into` with the value `V` holds, reusing its buffer.
    #[inline(always)]
    pub(crate) fn value_into(&self, v: &[u8], into: &mut IndexValue) {
        match (&self.form, &mut *into) {
            (Form::I64, IndexValue::I64(x)) => *x = (be8(v) ^ (1 << 63)) as i64,
            (Form::Composite(_), IndexValue::Str(s)) => {
                s.clear();
                s.extend_from_slice(v);
            }
            (Form::Str | Form::Unset, IndexValue::Str(s)) => {
                s.clear();
                unframe_into(v, s);
            }
            _ => *into = self.value(v),
        }
    }

    /// Write the key `H` stands for into `into`.
    #[inline(always)]
    pub(crate) fn key_into(&self, h: &[u8], into: &mut Vec<u8>) {
        // a buffer that already starts with the prefix keeps it
        if into.len() >= self.prefix.len() && into.starts_with(&self.prefix) {
            into.truncate(self.prefix.len());
        } else {
            into.clear();
            into.extend_from_slice(&self.prefix);
        }
        if self.digits {
            unpack_digits(h, into);
        } else {
            into.extend_from_slice(h);
        }
    }
}

/// The IEEE total-order transform: a negative float's magnitude grows
/// with its bits, so inverting them drops it below the positives.
fn f64_order(f: f64) -> u64 {
    let b = f.to_bits();
    if b >> 63 == 1 { !b } else { b | (1 << 63) }
}

fn f64_from_order(m: u64) -> f64 {
    f64::from_bits(if m >> 63 == 1 { m & !(1 << 63) } else { !m })
}

pub(crate) fn be8(b: &[u8]) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[..8]);
    u64::from_be_bytes(a)
}

fn frame(s: &[u8], out: &mut Vec<u8>) {
    for &b in s {
        out.push(b);
        if b == 0 {
            out.push(0xFF);
        }
    }
    out.extend_from_slice(&[0, 0]);
}

fn unframe_into(mut v: &[u8], out: &mut Vec<u8>) {
    // copy the runs between zero bytes whole; a zero is an escape or the end
    while let Some(z) = v.iter().position(|&b| b == 0) {
        out.extend_from_slice(&v[..z]);
        if v.get(z + 1) != Some(&0xFF) {
            return;
        }
        out.push(0);
        v = &v[z + 2..];
    }
}

/// Length of the framed string at the start of `e`, terminator included;
/// `inverted` reads a DESC component, whose bytes are complemented.
fn framed_len(e: &[u8], inverted: bool) -> usize {
    let z = if inverted { 0xFF } else { 0 };
    let mut i = 0;
    // jump from one marker byte to the next: an escape or the terminator
    loop {
        i += e[i..].iter().position(|&b| b == z).expect("a framed string ends");
        if e[i + 1] == z {
            return i + 2;
        }
        i += 2;
    }
}

fn component_len(e: &[u8], ty: ValType, order: SortOrder) -> usize {
    match ty {
        ValType::Str => framed_len(e, order == SortOrder::Desc),
        _ => 8,
    }
}

/// Pack a digit string two digits a byte, order-preserving.
pub(crate) fn pack_digits(d: &[u8], out: &mut Vec<u8>) {
    for pair in d.chunks(2) {
        let hi = pair[0] - b'0' + 1;
        let lo = pair.get(1).map_or(0, |c| c - b'0' + 1);
        out.push(hi << 4 | lo);
    }
}

/// Spread the 8 bytes of `x` to the even bytes of a `u128`.
#[inline(always)]
fn spread(x: u64) -> u128 {
    let mut v = u128::from(x);
    v = (v | (v << 32)) & 0x0000_0000_FFFF_FFFF_0000_0000_FFFF_FFFF;
    v = (v | (v << 16)) & 0x0000_FFFF_0000_FFFF_0000_FFFF_0000_FFFF;
    (v | (v << 8)) & 0x00FF_00FF_00FF_00FF_00FF_00FF_00FF_00FF
}

#[inline(always)]
pub(crate) fn unpack_digits(p: &[u8], out: &mut Vec<u8>) {
    let Some(&last) = p.last() else { return };
    // eight packed bytes at a time: high nibbles to the even output bytes,
    // low nibbles to the odd ones, then nibble n + 1 becomes digit n
    for chunk in p.chunks(8) {
        // assembled in a register: a stack copy reloaded wider stalls
        let x = chunk.iter().rev().fold(0u64, |x, &b| x << 8 | u64::from(b));
        let hi = spread((x >> 4) & 0x0F0F_0F0F_0F0F_0F0F);
        let lo = spread(x & 0x0F0F_0F0F_0F0F_0F0F);
        let d = (hi | (lo << 8)) + 0x2F2F_2F2F_2F2F_2F2F_2F2F_2F2F_2F2F_2F2F;
        out.extend_from_slice(&d.to_le_bytes()[..2 * chunk.len()]);
    }
    if last & 0x0F == 0 {
        out.pop();
    }
}

/// Append a LEB128 varint.
pub(crate) fn put_varint(mut n: usize, out: &mut Vec<u8>) {
    while n >= 0x80 {
        out.push((n as u8) | 0x80);
        n >>= 7;
    }
    out.push(n as u8);
}

/// Read a LEB128 varint at `*at`, advancing it.
#[inline]
pub(crate) fn varint(b: &[u8], at: &mut usize) -> usize {
    let first = b[*at];
    if first < 0x80 {
        *at += 1;
        return usize::from(first);
    }
    varint_long(b, at)
}

fn varint_long(b: &[u8], at: &mut usize) -> usize {
    let mut n = 0usize;
    let mut shift = 0;
    loop {
        let byte = b[*at];
        *at += 1;
        n |= usize::from(byte & 0x7F) << shift;
        if byte < 0x80 {
            return n;
        }
        shift += 7;
    }
}

/// Append one stored column.
pub(crate) fn put_column(v: Option<&[u8]>, out: &mut Vec<u8>) {
    match v {
        None => out.push(0),
        Some(b) if !b.is_empty() && b.iter().all(u8::is_ascii_digit) => {
            put_varint(2 * b.len() + 1, out);
            pack_digits(b, out);
        }
        Some(b) => {
            put_varint(2 * b.len() + 2, out);
            out.extend_from_slice(b);
        }
    }
}

/// One stored column read back without copying where it can be.
#[derive(Debug)]
pub(crate) enum Column<'a> {
    Absent,
    Plain(&'a [u8]),
    Digits(&'a [u8]),
}

impl<'a> Column<'a> {
    /// The column's bytes, decoded into `buf` when packed.
    pub(crate) fn bytes<'b>(self, buf: &'b mut Vec<u8>) -> Option<&'b [u8]>
    where
        'a: 'b,
    {
        match self {
            Column::Absent => None,
            Column::Plain(b) => Some(b),
            Column::Digits(p) => {
                buf.clear();
                unpack_digits(p, buf);
                Some(buf.as_slice())
            }
        }
    }

    pub(crate) fn into_vec(self) -> Option<Vec<u8>> {
        let mut buf = Vec::new();
        self.bytes(&mut buf).map(<[u8]>::to_vec)
    }
}

/// The column starting at `*at` in `payload`, advancing past it.
pub(crate) fn column<'a>(payload: &'a [u8], at: &mut usize) -> Column<'a> {
    let t = varint(payload, at);
    if t == 0 {
        return Column::Absent;
    }
    let start = *at;
    if t % 2 == 1 {
        let n = (t - 1) / 2;
        *at += n.div_ceil(2);
        Column::Digits(&payload[start..*at])
    } else {
        *at += (t - 2) / 2;
        Column::Plain(&payload[start..*at])
    }
}

/// Column `field` of `payload`.
pub(crate) fn nth_column(payload: &[u8], field: usize) -> Column<'_> {
    let mut at = 0;
    for _ in 0..field {
        column(payload, &mut at);
    }
    column(payload, &mut at)
}

#[cfg(test)]
#[path = "seg_codec_tests.rs"]
mod tests;
