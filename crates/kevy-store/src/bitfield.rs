//! `BITFIELD`: integers of 1 to 64 bits read from and written to any bit
//! offset of a string, with Redis's overflow rules.

#[cfg(not(feature = "std"))]
use crate::nostd_prelude::*;
use crate::{Store, StoreError};
use alloc::borrow::Cow;

/// A field's width and signedness: `i1`..`i64` or `u1`..`u63`.
///
/// ```
/// use kevy_store::BitType;
/// assert_eq!(BitType::parse(b"i8"), Some(BitType::signed(8)));
/// assert_eq!(BitType::parse(b"u64"), None, "u64 does not fit an i64 reply");
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BitType {
    signed: bool,
    bits: u8,
}

impl BitType {
    /// A two's complement field of `bits` width, 1 to 64.
    ///
    /// ```
    /// assert_eq!(kevy_store::BitType::parse(b"i64"), Some(kevy_store::BitType::signed(64)));
    /// ```
    pub const fn signed(bits: u8) -> Self {
        Self { signed: true, bits }
    }

    /// An unsigned field of `bits` width, 1 to 63.
    ///
    /// ```
    /// assert_eq!(kevy_store::BitType::parse(b"u8"), Some(kevy_store::BitType::unsigned(8)));
    /// ```
    pub const fn unsigned(bits: u8) -> Self {
        Self { signed: false, bits }
    }

    /// The width in bits.
    ///
    /// ```
    /// assert_eq!(kevy_store::BitType::signed(16).bits(), 16);
    /// ```
    pub const fn bits(self) -> u8 {
        self.bits
    }

    /// The type an argument spells, lower-case letter first.
    ///
    /// ```
    /// use kevy_store::BitType;
    /// assert_eq!(BitType::parse(b"u63"), Some(BitType::unsigned(63)));
    /// assert_eq!(BitType::parse(b"U8"), None);
    /// assert_eq!(BitType::parse(b"i0"), None);
    /// ```
    pub fn parse(b: &[u8]) -> Option<Self> {
        let (signed, digits) = match b {
            [b'i', rest @ ..] => (true, rest),
            [b'u', rest @ ..] => (false, rest),
            _ => return None,
        };
        let bits: u8 = core::str::from_utf8(digits).ok()?.parse().ok()?;
        let max = if signed { 64 } else { 63 };
        (1..=max).contains(&bits).then_some(Self { signed, bits })
    }
}

/// What a write does when its result does not fit the field.
///
/// ```
/// use kevy_store::{BitFieldOp, BitType, Overflow};
/// let mut s = kevy_store::Store::new();
/// let op = |of| [BitFieldOp::Set(BitType::unsigned(8), 0, 300, of)];
/// s.bitfield(b"k", &op(Overflow::Sat))?;
/// let (old, _) = s.bitfield(b"k", &op(Overflow::Wrap))?;
/// assert_eq!(old, [Some(255)], "SAT clamped 300 to 255");
/// # Ok::<(), kevy_store::StoreError>(())
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Overflow {
    /// Keep the low bits (the default).
    ///
    /// ```
    /// let (s, u8_) = (&mut kevy_store::Store::new(), kevy_store::BitType::unsigned(8));
    /// let set = kevy_store::BitFieldOp::Set(u8_, 0, 300, kevy_store::Overflow::Wrap);
    /// s.bitfield(b"k", &[set])?;
    /// assert_eq!(s.bitfield(b"k", &[kevy_store::BitFieldOp::Get(u8_, 0)])?.0, [Some(44)]);
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    Wrap,
    /// Clamp to the field's range.
    ///
    /// ```
    /// let (s, i8_) = (&mut kevy_store::Store::new(), kevy_store::BitType::signed(8));
    /// let incr = kevy_store::BitFieldOp::IncrBy(i8_, 0, -200, kevy_store::Overflow::Sat);
    /// assert_eq!(s.bitfield(b"k", &[incr])?.0, [Some(-128)]);
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    Sat,
    /// Leave the field alone and answer null.
    ///
    /// ```
    /// let (s, u8_) = (&mut kevy_store::Store::new(), kevy_store::BitType::unsigned(8));
    /// let incr = kevy_store::BitFieldOp::IncrBy(u8_, 0, 300, kevy_store::Overflow::Fail);
    /// assert_eq!(s.bitfield(b"k", &[incr])?, (vec![None], false));
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    Fail,
}

/// One `BITFIELD` operation, at a bit offset.
///
/// ```
/// use kevy_store::{BitFieldOp, BitType, Overflow};
/// let (mut s, u4) = (kevy_store::Store::new(), BitType::unsigned(4));
/// let (got, _) = s.bitfield(b"k", &[
///     BitFieldOp::IncrBy(u4, 4, 9, Overflow::Wrap),
///     BitFieldOp::Get(BitType::unsigned(8), 0),
/// ])?;
/// assert_eq!(got, [Some(9), Some(9)]);
/// # Ok::<(), kevy_store::StoreError>(())
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum BitFieldOp {
    /// Read the field.
    ///
    /// ```
    /// let mut s = kevy_store::Store::new();
    /// s.set(b"k", b"a".to_vec(), None, kevy_store::SetCondition::Always);
    /// let get = kevy_store::BitFieldOp::Get(kevy_store::BitType::unsigned(8), 0);
    /// assert_eq!(s.bitfield(b"k", &[get])?.0, [Some(97)]);
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    Get(BitType, u64),
    /// Write `value`, answering what was there.
    ///
    /// ```
    /// let mut s = kevy_store::Store::new();
    /// s.set(b"k", b"a".to_vec(), None, kevy_store::SetCondition::Always);
    /// let set = kevy_store::BitFieldOp::Set(kevy_store::BitType::unsigned(8), 0, 98, kevy_store::Overflow::Wrap);
    /// assert_eq!(s.bitfield(b"k", &[set])?.0, [Some(97)]);
    /// assert_eq!(s.get(b"k")?.as_deref(), Some(&b"b"[..]));
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    Set(BitType, u64, i64, Overflow),
    /// Add `incr`, answering the result.
    ///
    /// ```
    /// let mut s = kevy_store::Store::new();
    /// let incr = kevy_store::BitFieldOp::IncrBy(kevy_store::BitType::signed(16), 8, -5, kevy_store::Overflow::Wrap);
    /// assert_eq!(s.bitfield(b"k", &[incr, incr])?.0, [Some(-5), Some(-10)]);
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    IncrBy(BitType, u64, i64, Overflow),
}

fn read(bytes: &[u8], off: u64, t: BitType) -> i64 {
    let mut v: u64 = 0;
    for i in 0..u64::from(t.bits) {
        let pos = off + i;
        let byte = bytes.get((pos / 8) as usize).copied().unwrap_or(0);
        v = (v << 1) | u64::from((byte >> (7 - pos % 8)) & 1);
    }
    if t.signed && t.bits < 64 && v >> (t.bits - 1) & 1 == 1 {
        v |= u64::MAX << t.bits;
    }
    v as i64
}

fn write(bytes: &mut [u8], off: u64, t: BitType, v: i64) {
    let v = v as u64;
    for i in 0..u64::from(t.bits) {
        let pos = off + i;
        let bit = (v >> (u64::from(t.bits) - 1 - i)) & 1;
        let (idx, shift) = ((pos / 8) as usize, 7 - pos % 8);
        bytes[idx] = (bytes[idx] & !(1 << shift)) | ((bit as u8) << shift);
    }
}

/// `value + incr` kept to the field by `of`, or `None` for a failing
/// overflow. A `Set` is an add to nothing, of the value cast as Redis
/// casts it for an unsigned field.
fn fit(value: i64, incr: i64, t: BitType, of: Overflow) -> Option<i64> {
    let bits = u32::from(t.bits);
    let (lo, hi, start): (i128, i128, i128) = if t.signed {
        (-(1i128 << (bits - 1)), (1i128 << (bits - 1)) - 1, i128::from(value))
    } else {
        (0, (1i128 << bits) - 1, i128::from(value as u64))
    };
    let sum = start + i128::from(incr);
    if (lo..=hi).contains(&sum) {
        return Some(sum as i64);
    }
    match of {
        Overflow::Fail => None,
        Overflow::Sat => Some(if sum > hi { hi } else { lo } as i64),
        Overflow::Wrap => {
            let low = (sum as u128 as u64) & if bits == 64 { u64::MAX } else { (1u64 << bits) - 1 };
            let mut bytes = [0u8; 8];
            write(&mut bytes, 0, t, low as i64);
            Some(read(&bytes, 0, t))
        }
    }
}

impl Store {
    /// `BITFIELD` — run `ops` in order against the string at `key`, one
    /// answer each (`None` for a write its overflow rule refused). Any
    /// write lengthens the string to cover every write's field first,
    /// whether or not the write goes through, as Redis does; reads past the
    /// end read zeros. Whether a write went through comes back too.
    ///
    /// ```
    /// use kevy_store::{BitFieldOp, BitType, Overflow};
    /// let mut s = kevy_store::Store::new();
    /// let u8_ = BitType::unsigned(8);
    /// let (got, wrote) = s.bitfield(b"k", &[
    ///     BitFieldOp::Set(u8_, 0, 200, Overflow::Wrap),
    ///     BitFieldOp::IncrBy(u8_, 0, 100, Overflow::Sat),
    ///     BitFieldOp::IncrBy(u8_, 0, 1, Overflow::Fail),
    ///     BitFieldOp::Get(u8_, 0),
    /// ])?;
    /// assert_eq!(got, [Some(0), Some(255), None, Some(255)]);
    /// assert!(wrote);
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    pub fn bitfield(
        &mut self,
        key: &[u8],
        ops: &[BitFieldOp],
    ) -> Result<(Vec<Option<i64>>, bool), StoreError> {
        let mut got = Vec::with_capacity(ops.len());
        let wrote = self.bitfield_each(key, ops, |r| got.push(r))?;
        Ok((got, wrote))
    }

    /// [`Self::bitfield`], each operation's answer handed to `f` in order;
    /// whether anything was written. A write inside a bulk value only
    /// this key holds happens in place.
    ///
    /// ```
    /// use kevy_store::{BitFieldOp, BitType, Overflow, Store};
    /// let mut s = Store::new();
    /// let u8t = BitType::parse(b"u8").unwrap();
    /// let mut got = Vec::new();
    /// let ops = [BitFieldOp::IncrBy(u8t, 0, 5, Overflow::Wrap), BitFieldOp::Get(u8t, 0)];
    /// assert!(s.bitfield_each(b"k", &ops, |r| got.push(r))?);
    /// assert_eq!(got, [Some(5), Some(5)]);
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    pub fn bitfield_each(
        &mut self,
        key: &[u8],
        ops: &[BitFieldOp],
        mut f: impl FnMut(Option<i64>),
    ) -> Result<bool, StoreError> {
        let reach = ops
            .iter()
            .filter_map(|op| match *op {
                BitFieldOp::Set(t, off, ..) | BitFieldOp::IncrBy(t, off, ..) => {
                    Some(off + u64::from(t.bits))
                }
                BitFieldOp::Get(..) => None,
            })
            .max();
        let Some(reach) = reach else {
            let bytes = self.get(key)?.unwrap_or(Cow::Borrowed(&[]));
            for op in ops {
                f(match *op {
                    BitFieldOp::Get(t, off) => Some(read(&bytes, off, t)),
                    _ => None,
                });
            }
            return Ok(false);
        };
        let need = reach.div_ceil(8) as usize;
        if let Some(bytes) = self.bytes_in_place(key, need) {
            return Ok(apply(bytes, ops, f));
        }
        let mut bytes = self.get(key)?.map(Cow::into_owned).unwrap_or_default();
        if bytes.len() < need {
            bytes.resize(need, 0);
        }
        let wrote = apply(&mut bytes, ops, f);
        self.set_bytes_keep_ttl(key, bytes);
        Ok(wrote)
    }
}

/// Run `ops` over `bytes`, long enough for every write, each answer to
/// `f`; whether any wrote.
fn apply(bytes: &mut [u8], ops: &[BitFieldOp], mut f: impl FnMut(Option<i64>)) -> bool {
    let mut wrote = false;
    for op in ops {
        f(match *op {
            BitFieldOp::Get(t, off) => Some(read(bytes, off, t)),
            BitFieldOp::Set(t, off, v, of) => fit(v, 0, t, of).map(|nv| {
                let old = read(bytes, off, t);
                write(bytes, off, t, nv);
                wrote = true;
                old
            }),
            BitFieldOp::IncrBy(t, off, incr, of) => {
                fit(read(bytes, off, t), incr, t, of).inspect(|&nv| {
                    write(bytes, off, t, nv);
                    wrote = true;
                })
            }
        });
    }
    wrote
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lcg(seed: &mut u64) -> u64 {
        *seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        *seed >> 33
    }

    /// The field read bit by bit through `getbit`, signed by arithmetic.
    fn oracle(s: &mut Store, key: &[u8], off: u64, t: BitType) -> i64 {
        let mut v: i128 = 0;
        for i in 0..u64::from(t.bits) {
            v = v * 2 + i128::from(s.getbit(key, off + i).unwrap());
        }
        if t.signed && v >= 1i128 << (t.bits - 1) {
            v -= 1i128 << t.bits;
        }
        v as i64
    }

    fn random_type(seed: &mut u64) -> BitType {
        let signed = lcg(seed).is_multiple_of(2);
        let bits = 1 + (lcg(seed) % if signed { 64 } else { 63 }) as u8;
        if signed { BitType::signed(bits) } else { BitType::unsigned(bits) }
    }

    #[test]
    fn reads_and_writes_agree_with_the_bits() {
        let mut seed = 7;
        let mut s = Store::new();
        let init: Vec<u8> = (0..24).map(|_| lcg(&mut seed) as u8).collect();
        s.set(b"k", init, None, crate::SetCondition::Always);
        for _ in 0..2000 {
            let t = random_type(&mut seed);
            let off = lcg(&mut seed) % 200;
            let want = oracle(&mut s, b"k", off, t);
            let (got, _) = s.bitfield(b"k", &[BitFieldOp::Get(t, off)]).unwrap();
            assert_eq!(got, [Some(want)], "GET {t:?} at {off}");
            let v = lcg(&mut seed) as i64 - (1 << 30);
            let (old, _) = s.bitfield(b"k", &[BitFieldOp::Set(t, off, v, Overflow::Wrap)]).unwrap();
            assert_eq!(old, [Some(want)]);
            let low = (v as i128).rem_euclid(1i128 << t.bits);
            let mut expect = low;
            if t.signed && low >= 1i128 << (t.bits - 1) {
                expect -= 1i128 << t.bits;
            }
            assert_eq!(oracle(&mut s, b"k", off, t), expect as i64, "SET {t:?} {v} at {off}");
        }
    }

    #[test]
    fn overflow_at_the_widest_fields() {
        let (i64_, u63) = (BitType::signed(64), BitType::unsigned(63));
        let mut s = Store::new();
        let mut run = |ops: &[BitFieldOp]| s.bitfield(b"w", ops).unwrap().0;
        assert_eq!(run(&[BitFieldOp::Set(i64_, 0, i64::MAX, Overflow::Wrap)]), [Some(0)]);
        assert_eq!(run(&[BitFieldOp::IncrBy(i64_, 0, 1, Overflow::Wrap)]), [Some(i64::MIN)]);
        assert_eq!(run(&[BitFieldOp::IncrBy(i64_, 0, -1, Overflow::Sat)]), [Some(i64::MIN)]);
        assert_eq!(run(&[BitFieldOp::IncrBy(i64_, 0, -1, Overflow::Fail)]), [None]);
        let top = i64::MAX;
        assert_eq!(run(&[BitFieldOp::Set(u63, 64, top, Overflow::Wrap)]), [Some(0)]);
        assert_eq!(run(&[BitFieldOp::IncrBy(u63, 64, 1, Overflow::Wrap)]), [Some(0)]);
        assert_eq!(run(&[BitFieldOp::IncrBy(u63, 64, -1, Overflow::Sat)]), [Some(0)]);
        assert_eq!(run(&[BitFieldOp::IncrBy(u63, 64, -1, Overflow::Fail)]), [None]);
        // a negative value written to an unsigned field is its two's
        // complement, so it overflows the field
        let u8_ = BitType::unsigned(8);
        assert_eq!(run(&[BitFieldOp::Set(u8_, 200, -1, Overflow::Sat)]), [Some(0)]);
        assert_eq!(run(&[BitFieldOp::Get(u8_, 200)]), [Some(255)]);
    }

    #[test]
    fn a_refused_write_still_lengthens_and_reads_do_not_create() {
        let mut s = Store::new();
        let u8_ = BitType::unsigned(8);
        let (got, wrote) = s.bitfield(b"r", &[BitFieldOp::Get(u8_, 100)]).unwrap();
        assert_eq!((got, wrote, s.key_exists(b"r")), (vec![Some(0)], false, false));
        let (got, wrote) =
            s.bitfield(b"n", &[BitFieldOp::IncrBy(u8_, 16, 300, Overflow::Fail)]).unwrap();
        assert_eq!((got, wrote), (vec![None], false));
        assert_eq!(s.get(b"n").unwrap().as_deref(), Some(&[0u8, 0, 0][..]));
    }
}
