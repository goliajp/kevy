//! Bitmap ops on the embedded `Store` (kevy-embedded 1.8.0).
//!
//! Wraps the new `kevy_store::Store::{getbit, setbit, bitcount}`
//! methods. Strings act as bit arrays addressed MSB-first within
//! each byte, matching Redis semantics.
//!
//! Lives outside `ops.rs` / `ops_p2.rs` / `ops_p3.rs` to keep
//! every embedded file under the 500-LOC house rule.

use std::io;

#[cfg(not(target_arch = "wasm32"))]
use crate::replica_glue::ensure_writable;
use crate::store::{Store, commit_write, store_err};

#[cfg(target_arch = "wasm32")]
fn ensure_writable(_s: &Store) -> io::Result<()> { Ok(()) }

impl Store {
    /// `GETBIT key offset` — return the bit at `offset` (MSB-first).
    /// `0` for missing key or past-end.
    pub fn getbit(&self, key: &[u8], offset: u64) -> io::Result<u8> {
        self.wshard(key).store.getbit(key, offset).map_err(store_err)
    }

    /// `SETBIT key offset value` — set the bit at `offset` to
    /// `value` (0 or 1). Extends the underlying string with zero
    /// padding as needed. Returns the PREVIOUS bit value.
    pub fn setbit(&self, key: &[u8], offset: u64, value: u8) -> io::Result<u8> {
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
    pub fn bitcount(&self, key: &[u8], range: Option<(i64, i64)>) -> io::Result<u64> {
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
    ) -> io::Result<Option<u64>> {
        self.wshard(key)
            .store
            .bitpos(key, bit, range)
            .map_err(store_err)
    }

    /// `GETRANGE key start end` — substring with Redis negative
    /// indexing; `[start, end]` inclusive.
    pub fn getrange(&self, key: &[u8], start: i64, end: i64) -> io::Result<Vec<u8>> {
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
    ) -> io::Result<usize> {
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
}
