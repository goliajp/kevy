//! Bitmap reads, writes, and aggregates: `GETBIT` / `SETBIT` /
//! `BITCOUNT` / `BITPOS` / `BITOP` / `GETRANGE` / `SETRANGE`.
//!
//! Strings act as bit arrays addressed MSB-first within each byte,
//! matching Redis semantics.

use kevy_store::BitOp;
use crate::{KevyError, KevyResult};

use crate::store::ensure_writable;
use crate::store::{Store, commit_write, store_err};

impl Store {
    /// `GETBIT key offset` — return the bit at `offset` (MSB-first).
    /// `0` for missing key or past-end.
    pub fn getbit(&self, key: &[u8], offset: u64) -> KevyResult<u8> {
        self.wshard(key).store.getbit(key, offset).map_err(store_err)
    }

    /// `SETBIT key offset value` — set the bit at `offset` to
    /// `value` (0 or 1). Extends the underlying string with zero
    /// padding as needed. Returns the PREVIOUS bit value.
    pub fn setbit(&self, key: &[u8], offset: u64, value: u8) -> KevyResult<u8> {
        ensure_writable(self)?;
        let mut g = self.wshard(key);
        let prev = g.store.setbit(key, offset, value).map_err(store_err)?;
        let off_str = format!("{offset}");
        let val_str = format!("{value}");
        commit_write(&mut g, &[b"SETBIT", key, off_str.as_bytes(), val_str.as_bytes()])?;
        Ok(prev)
    }

    /// `BITCOUNT key [start end]` — count set bits over the
    /// optional byte-offset range (inclusive, negatives-from-tail
    /// like Redis). `None` for `range` = whole string.
    pub fn bitcount(&self, key: &[u8], range: Option<(i64, i64)>) -> KevyResult<u64> {
        self.wshard(key).store.bitcount(key, range).map_err(store_err)
    }

    /// `BITPOS key bit [start [end]]` — find first bit equal to
    /// `bit` (0 or 1) in the optional byte range. Returns `None`
    /// when not found (Redis would reply `:-1`).
    pub fn bitpos(
        &self,
        key: &[u8],
        bit: u8,
        range: Option<(i64, i64)>,
    ) -> KevyResult<Option<u64>> {
        self.wshard(key)
            .store
            .bitpos(key, bit, range)
            .map_err(store_err)
    }

    /// `GETRANGE key start end` — substring with Redis negative
    /// indexing; `[start, end]` inclusive.
    pub fn getrange(&self, key: &[u8], start: i64, end: i64) -> KevyResult<Vec<u8>> {
        self.wshard(key)
            .store
            .getrange(key, start, end)
            .map_err(store_err)
    }

    /// `SETRANGE key offset value` — overwrite bytes at `offset`;
    /// extends with zero padding if past current length. Returns
    /// the new total length.
    pub fn setrange(
        &self,
        key: &[u8],
        offset: u64,
        value: &[u8],
    ) -> KevyResult<usize> {
        ensure_writable(self)?;
        let mut g = self.wshard(key);
        let new_len = g
            .store
            .setrange(key, offset, value)
            .map_err(store_err)?;
        let off_str = format!("{offset}");
        commit_write(&mut g, &[b"SETRANGE", key, off_str.as_bytes(), value])?;
        Ok(new_len)
    }

    /// `BITOP AND|OR|XOR|NOT destkey srckey [srckey ...]` — bitwise
    /// op across N source keys, stored at `destkey`. Returns the
    /// destination string length (= longest source length, with
    /// shorter sources zero-padded). For `Not`, exactly one source
    /// key (additional ones are rejected).
    pub fn bitop(
        &self,
        op: BitOp,
        dst: &[u8],
        srcs: &[&[u8]],
    ) -> KevyResult<usize> {
        ensure_writable(self)?;
        if srcs.is_empty() {
            return Ok(0);
        }
        if matches!(op, BitOp::Not) && srcs.len() != 1 {
            return Err(KevyError::InvalidInput("BITOP NOT takes exactly one source key".into()));
        }
        // Read each source (own each as Vec<u8>) — set-algebra style.
        let mut srcs_bytes: Vec<Vec<u8>> = Vec::with_capacity(srcs.len());
        for k in srcs {
            let v = self.get(k)?.unwrap_or_default();
            srcs_bytes.push(v);
        }
        let max_len = srcs_bytes.iter().map(Vec::len).max().unwrap_or(0);
        if max_len == 0 {
            // Empty result — delete dst.
            self.del(&[dst])?;
            return Ok(0);
        }
        let out = kevy_store::bitop_combine(op, &srcs_bytes, max_len);
        // Write dst.
        self.set(dst, &out)?;
        Ok(max_len)
    }

    /// `TIME` — `(unix_seconds, microseconds)` tuple. Useful for
    /// time-based embedded logic + tracing.
    pub fn time(&self) -> (u64, u32) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        (now.as_secs(), now.subsec_micros())
    }
}
