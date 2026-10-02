//! INCRBYFLOAT and HINCRBYFLOAT, in the precision Redis computes them in on
//! x86-64: the stored text and the increment read as `long double`, added,
//! and the sum stored as `%.17Lf` with its trailing zeros dropped.

#[cfg(not(feature = "std"))]
use crate::nostd_prelude::*;
use kevy_num::LongDouble;

use crate::util::{format_i64_into, itoa_i64_stack};
use crate::value::{SmallBytes, Value};
use crate::{Entry, Store, StoreError};

/// The text Redis stores for `v`: `%.17Lf` without trailing zeros or a
/// bare point, and a negative zero as `0`.
fn human(v: LongDouble) -> Vec<u8> {
    let mut out = Vec::with_capacity(24);
    v.write_fixed(&mut out, 17);
    if out.contains(&b'.') {
        while out.last() == Some(&b'0') {
            out.pop();
        }
        if out.last() == Some(&b'.') {
            out.pop();
        }
    }
    if out == b"-0" {
        out.remove(0);
    }
    out
}

fn read(b: &[u8], refused: StoreError) -> Result<LongDouble, StoreError> {
    LongDouble::parse_exact(b).ok_or(refused)
}

impl Store {
    /// `INCRBYFLOAT key delta` with the increment as given: the stored
    /// value's new text. TTL kept.
    ///
    /// ```
    /// let mut s = kevy_store::Store::new();
    /// assert_eq!(s.incr_by_float_text(b"k", b"1.1").unwrap(), b"1.1");
    /// assert_eq!(s.incr_by_float_text(b"k", b"2.2").unwrap(), b"3.3");
    /// assert_eq!(s.incr_by_float_text(b"k", b"0x10").unwrap(), b"19.3");
    /// ```
    pub fn incr_by_float_text(&mut self, key: &[u8], delta: &[u8]) -> Result<Vec<u8>, StoreError> {
        self.incr_float(key, || read(delta, StoreError::NotFloat))
    }

    /// `INCRBYFLOAT` with a double increment, read as the shortest decimal
    /// that names it — the text a log of this call carries, so a replay
    /// reads the same number. See [`Self::incr_by_float_text`].
    ///
    /// ```
    /// let mut s = kevy_store::Store::new();
    /// assert_eq!(s.incr_by_float(b"k", 0.1).unwrap(), b"0.1");
    /// assert_eq!(s.incr_by_float(b"k", 0.2).unwrap(), b"0.3");
    /// ```
    pub fn incr_by_float(&mut self, key: &[u8], delta: f64) -> Result<Vec<u8>, StoreError> {
        self.incr_by_float_text(key, alloc::format!("{delta}").as_bytes())
    }

    /// The key's type first, then its value, then the increment, as Redis
    /// checks them.
    fn incr_float(
        &mut self,
        key: &[u8],
        delta: impl FnOnce() -> Result<LongDouble, StoreError>,
    ) -> Result<Vec<u8>, StoreError> {
        self.tier_resolve(key, crate::value::COLD_TAG_STRING)?;
        let cur = match self.live_entry(key).map(|e| &e.value) {
            None => LongDouble::from(0.0),
            Some(Value::Str(v)) => read(v.as_slice(), StoreError::NotFloat)?,
            Some(Value::ArcBulk(a)) => read(a.as_ref(), StoreError::NotFloat)?,
            Some(Value::Int(n)) => {
                read(format_i64_into(*n, &mut itoa_i64_stack()), StoreError::NotFloat)?
            }
            Some(_) => return Err(StoreError::WrongType),
        };
        let next = cur + delta()?;
        if !next.is_finite() {
            return Err(StoreError::IncrNotFinite);
        }
        let bytes = human(next);
        match self.live_entry_mut(key) {
            Some(e) => {
                let before = e.value.weight();
                e.value = Value::Str(SmallBytes::from_slice(&bytes));
                self.reweigh_scalar(key, before);
            }
            None => {
                self.insert_entry(
                    SmallBytes::from_slice(key),
                    Entry::new(Value::Str(SmallBytes::from_slice(&bytes)), None),
                );
            }
        }
        Ok(bytes)
    }

    /// `HINCRBYFLOAT key field delta` with the increment as given: the
    /// field's new text. The field's TTL is kept.
    ///
    /// ```
    /// let mut s = kevy_store::Store::new();
    /// assert_eq!(s.hincrbyfloat_text(b"h", b"f", b"0.1").unwrap(), b"0.1");
    /// assert_eq!(s.hincrbyfloat_text(b"h", b"f", b"0.2").unwrap(), b"0.3");
    /// ```
    pub fn hincrbyfloat_text(
        &mut self,
        key: &[u8],
        field: &[u8],
        delta: &[u8],
    ) -> Result<Vec<u8>, StoreError> {
        let d = read(delta, StoreError::NotFloat)?;
        self.hincr_float(key, field, d)
    }

    /// `HINCRBYFLOAT` with a double increment, read as the shortest
    /// decimal that names it (see [`Self::incr_by_float`]): the new value,
    /// read back as a double from the text stored.
    ///
    /// ```
    /// let mut s = kevy_store::Store::new();
    /// assert_eq!(s.hincrbyfloat(b"h", b"f", 1.5).unwrap(), 1.5);
    /// s.hincrbyfloat(b"h", b"g", 1e308).unwrap();
    /// assert_eq!(s.hincrbyfloat(b"h", b"g", 1e308).unwrap(), f64::INFINITY, "2e308 is a long double");
    /// ```
    pub fn hincrbyfloat(
        &mut self,
        key: &[u8],
        field: &[u8],
        delta: f64,
    ) -> Result<f64, StoreError> {
        let text = self.hincrbyfloat_text(key, field, alloc::format!("{delta}").as_bytes())?;
        // a sum past a double's range reads as an infinity, as C reads it
        Ok(kevy_num::strtod(&text).value)
    }

    fn hincr_float(
        &mut self,
        key: &[u8],
        field: &[u8],
        d: LongDouble,
    ) -> Result<Vec<u8>, StoreError> {
        if !d.is_finite() {
            return Err(StoreError::ValueNotFinite);
        }
        self.purge_hash_ttl(key);
        let (bytes, weight_delta) = {
            let mut h = self.hash_mut(key, true)?.expect("created");
            let cur = match h.get(field) {
                Some(v) => read(v.as_slice(), StoreError::HashValueNotFloat)?,
                None => LongDouble::from(0.0),
            };
            let next = cur + d;
            if !next.is_finite() {
                return Err(StoreError::IncrNotFinite);
            }
            let bytes = human(next);
            let (_, wd) =
                h.insert_weighed(SmallBytes::from_slice(field), SmallBytes::from_slice(&bytes));
            (bytes, wd)
        };
        self.account_delta(key, weight_delta);
        Ok(bytes)
    }
}
