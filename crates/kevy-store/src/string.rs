//! `Store` string read commands (GET family) + INCRBY. The SET-family
//! write path lives in `string_set.rs` (500-LOC house cap).

#[cfg(not(feature = "std"))]
use crate::nostd_prelude::*;
use crate::util::{format_i64_into, itoa_i64_stack, parse_i64};
use crate::value::{SmallBytes, Value};
use crate::{Entry, Store, StoreError};
use alloc::borrow::Cow;
use alloc::sync::Arc;

/// L1 return shape for [`Store::get_for_reply`] — lets the reactor's reply
/// path choose between memcpy (`Bytes`) and writev zero-copy (`ArcBulk`)
/// off one keyspace lookup.
pub enum GetReply<'a> {
    /// Inline-encoded value — caller memcpys the bytes into its output Vec
    /// (small replies; encoding cost is tiny vs the RTT floor).
    Bytes(Cow<'a, [u8]>),
    /// Arc-backed bulk. The reactor's reply path pushes
    /// the Arc into the conn's `output_arcs` so the next `writev` iovec
    /// list points DIRECTLY at the value bytes — skipping the per-GET
    /// memcpy that valkey's `tryAvoidBulkStrCopyToReply` likewise avoids.
    ArcBulk(Arc<Box<[u8]>>),
}

/// Owned GET result for the FFI zero-copy shared lane
/// ([`Store::get_shared_owned`]). Bulk values ride out as an Arc clone (no
/// byte copy); small values as a plain Vec (one alloc — cheaper than a fresh
/// Arc). The FFI's shared free reconstructs whichever the tag says.
pub enum GetShared {
    /// Bulk value — the engine's Arc, cloned. Zero byte copy.
    Arc(Arc<Box<[u8]>>),
    /// Small value (Str/Int) — a plain owned Vec, one allocation.
    Bytes(Vec<u8>),
}

impl Store {
    // ---- strings -------------------------------------------------------
    /// GET variant that exposes the underlying encoding
    /// so the reactor's reply path can choose zero-copy
    /// (`Value::ArcBulk` → push the Arc to the conn's `output_arcs` for a
    /// writev iovec) vs memcpy (`Value::Str` / `Value::Int` → encode bytes
    /// into the conn's output Vec). ONE keyspace lookup; the variant tag
    /// chooses the encoding without a second probe.
    pub fn get_for_reply(&mut self, key: &[u8]) -> Result<Option<GetReply<'_>>, StoreError> {
        match self.tier_serve(key, crate::value::COLD_TAG_STRING)? {
            None => Ok(None),
            Some(e) => match &e.value {
                Value::Str(v) => Ok(Some(GetReply::Bytes(Cow::Borrowed(v.as_slice())))),
                Value::ArcBulk(a) => Ok(Some(GetReply::ArcBulk(Arc::clone(a)))),
                Value::Int(n) => {
                    let mut tmp = itoa_i64_stack();
                    let s = format_i64_into(*n, &mut tmp);
                    Ok(Some(GetReply::Bytes(Cow::Owned(s.to_vec()))))
                }
                _ => Err(StoreError::WrongType),
            },
        }
    }

    /// Owned GET for the FFI scalar *shared* lane (`kevy_get_shared`). Bulk
    /// values (`Value::ArcBulk`) return an `Arc::clone` — **no byte copy**; the
    /// FFI holds the Arc alive and hands JS a buffer that views it directly
    /// (mirrors MMKV's zero-copy mmap-page view, the thing that made kevy lose
    /// large GET). Small values (`Str`/`Int`) allocate a fresh `Arc<Box<[u8]>>`
    /// — the same one copy the Vec lane already pays — so the caller's free path
    /// is uniform. Read-only (`&self`, like [`Self::get_shared`]) so the FFI
    /// can take it under a SHARED shard lock — no LRU stamp, matching the
    /// `maxmemory == 0` fast path the mobile door runs on. Wrong type errors
    /// like [`Self::get_for_reply`].
    pub fn get_shared_owned(&self, key: &[u8]) -> Result<Option<GetShared>, StoreError> {
        match self.map.get(key) {
            None => Ok(None),
            Some(e) if e.is_expired(self.cached_clock, self.cached_ns) => Ok(None),
            // Bulk (already Arc-backed) → clone the Arc: ZERO byte copy. Small
            // (`Str`/`Int`) → a plain Vec (one alloc, cheaper than wrapping a
            // fresh Arc — the FFI's shared free handles either).
            Some(e) => match &e.value {
                Value::ArcBulk(a) => Ok(Some(GetShared::Arc(Arc::clone(a)))),
                Value::Str(v) => Ok(Some(GetShared::Bytes(v.as_slice().to_vec()))),
                Value::Int(n) => {
                    let mut tmp = itoa_i64_stack();
                    let s = format_i64_into(*n, &mut tmp);
                    Ok(Some(GetShared::Bytes(s.to_vec())))
                }
                // Cold, `&self` shared lane: pread a fresh value — never
                // promotes, never sets the probation mark (it cannot:
                // no `&mut`). Documented: shared-lane reads pay a pread
                // until a `&mut`-path access promotes the key.
                Value::Cold(c) if c.type_tag == crate::value::COLD_TAG_STRING => {
                    match self.tier_peek_value(key, &e.value).expect("cold peek") {
                        Value::ArcBulk(a) => Ok(Some(GetShared::Arc(a))),
                        v => Ok(Some(GetShared::Bytes(cold_string_bytes(&v)))),
                    }
                }
                _ => Err(StoreError::WrongType),
            },
        }
    }

    /// Fused GET-into-output. Skips the [`GetReply`] enum tag
    /// round-trip + caller match arm by writing the RESP frame directly into
    /// `output` (header + bytes + CRLF for Str/Int) or pushing the Arc into
    /// `output_arcs` at the right offset (ArcBulk zero-copy via writev).
    /// Returns the same outcomes as [`Self::get_for_reply`]: `Ok(true)` if
    /// the key was found and emitted, `Ok(false)` if absent (the caller
    /// emits the `$-1` null bulk — preserves the existing inline-null
    /// semantics on the reactor side), `Err` for WRONGTYPE.
    pub fn get_into_output(
        &mut self,
        key: &[u8],
        output: &mut Vec<u8>,
        output_arcs: &mut Vec<(usize, Arc<Box<[u8]>>)>,
    ) -> Result<bool, StoreError> {
        match self.tier_serve(key, crate::value::COLD_TAG_STRING)? {
            None => Ok(false),
            Some(e) => match &e.value {
                Value::Str(v) => {
                    let bytes = v.as_slice();
                    crate::util::bulk_header_into(output, bytes.len());
                    output.extend_from_slice(bytes);
                    output.extend_from_slice(b"\r\n");
                    Ok(true)
                }
                Value::ArcBulk(a) => {
                    crate::util::bulk_header_into(output, a.len());
                    let pos = output.len();
                    output_arcs.push((pos, Arc::clone(a)));
                    output.extend_from_slice(b"\r\n");
                    Ok(true)
                }
                Value::Int(n) => {
                    let mut tmp = itoa_i64_stack();
                    let s = format_i64_into(*n, &mut tmp);
                    crate::util::bulk_header_into(output, s.len());
                    output.extend_from_slice(s);
                    output.extend_from_slice(b"\r\n");
                    Ok(true)
                }
                _ => Err(StoreError::WrongType),
            },
        }
    }

    /// `GET` — returns a `Cow<[u8]>` so `Value::Int` callers can format the
    /// integer to ASCII without storing it. `Value::Str`
    /// returns `Cow::Borrowed` (zero copy); `Value::Int`
    /// formats to a small owned `Vec<u8>` (up to 20 bytes for `i64::MIN`).
    pub fn get(&mut self, key: &[u8]) -> Result<Option<Cow<'_, [u8]>>, StoreError> {
        match self.tier_serve(key, crate::value::COLD_TAG_STRING)? {
            None => Ok(None),
            Some(e) => match &e.value {
                Value::Str(v) => Ok(Some(Cow::Borrowed(v.as_slice()))),
                // L1: Arc-backed bulk — return borrow into the Arc's
                // bytes. Caller can either memcpy via Cow::Borrowed
                // (default `encode_bulk` path) OR look up the
                // underlying `Value::ArcBulk(arc)` separately for the
                // writev zero-copy reply path.
                Value::ArcBulk(a) => Ok(Some(Cow::Borrowed(a.as_ref()))),
                Value::Int(n) => {
                    let mut tmp = itoa_i64_stack();
                    let s = format_i64_into(*n, &mut tmp);
                    Ok(Some(Cow::Owned(s.to_vec())))
                }
                _ => Err(StoreError::WrongType),
            },
        }
    }

    /// Read-only `GET`: `&self`, so concurrent readers can run under a shared
    /// lock (embedded mode's `RwLock` read path). Expiry is checked against the
    /// coarse cached clock but an expired key is *not* removed here (no `&mut`)
    /// — the reaper / next write reclaims it; a reader just sees `None`. LRU is
    /// not touched, so this path is only used when eviction is off
    /// (`maxmemory == 0`); with eviction, the caller takes the mutating
    /// [`Self::get`] under an exclusive lock so access still stamps the LRU.
    pub fn get_shared(&self, key: &[u8]) -> Result<Option<Cow<'_, [u8]>>, StoreError> {
        match self.map.get(key) {
            None => Ok(None),
            Some(e) if e.is_expired(self.cached_clock, self.cached_ns) => Ok(None),
            Some(e) => match &e.value {
                Value::Str(v) => Ok(Some(Cow::Borrowed(v.as_slice()))),
                Value::ArcBulk(a) => Ok(Some(Cow::Borrowed(a.as_ref()))),
                Value::Int(n) => {
                    let mut tmp = itoa_i64_stack();
                    let s = format_i64_into(*n, &mut tmp);
                    Ok(Some(Cow::Owned(s.to_vec())))
                }
                // Cold, `&self` shared lane — see `get_shared_owned`.
                Value::Cold(c) if c.type_tag == crate::value::COLD_TAG_STRING => {
                    let v = self.tier_peek_value(key, &e.value).expect("cold peek");
                    Ok(Some(Cow::Owned(cold_string_bytes(&v))))
                }
                _ => Err(StoreError::WrongType),
            },
        }
    }

    /// Byte length of a string value. A missing key is 0, matching
    /// STRLEN; an integer-encoded value reports the length it would
    /// format to, not 8.
    pub fn strlen(&mut self, key: &[u8]) -> Result<usize, StoreError> {
        Ok(self.get(key)?.map_or(0, |c| c.len()))
    }

    /// `INCRBY` family; preserves any TTL.
    ///
    /// Following valkey's OBJ_ENCODING_INT approach: the hot path
    /// matches `Value::Int(n)` and does the increment in place — no parse,
    /// no format, no allocation. The `Value::Str` arm parses,
    /// increments, and **promotes** to `Value::Int(next)` so subsequent
    /// INCRs land on the fast path. Insert-new path also lands as `Int`.
    pub fn incr_by(&mut self, key: &[u8], delta: i64) -> Result<i64, StoreError> {
        self.tier_resolve(key, crate::value::COLD_TAG_STRING)?; // cold string pages in

        let outcome = match self.live_entry_mut(key) {
            Some(e) => match &mut e.value {
                Value::Int(n) => {
                    let next = n.checked_add(delta).ok_or(StoreError::Overflow)?;
                    *n = next;
                    // In-place i64 mutation — weight unchanged (still 0
                    // heap bytes for an Int). Skip the reweigh entirely.
                    return Ok(next);
                }
                Value::Str(v) => {
                    let next = parse_i64(v.as_slice())
                        .ok_or(StoreError::NotInteger)?
                        .checked_add(delta)
                        .ok_or(StoreError::Overflow)?;
                    // Promote to Int: future INCRs hit the fast path.
                    e.value = Value::Int(next);
                    IncrOutcome::Reweigh(next)
                }
                Value::ArcBulk(a) => {
                    // L1: large value claimed to be numeric — parse and
                    // promote to Int. Subsequent INCRs hit the fast path.
                    let next = parse_i64(a.as_ref())
                        .ok_or(StoreError::NotInteger)?
                        .checked_add(delta)
                        .ok_or(StoreError::Overflow)?;
                    e.value = Value::Int(next);
                    IncrOutcome::Reweigh(next)
                }
                _ => return Err(StoreError::WrongType),
            },
            // Absent/expired ⇒ start from 0; 0 + delta can't overflow i64.
            None => IncrOutcome::Insert(delta),
        };
        match outcome {
            IncrOutcome::Reweigh(next) => {
                self.reweigh_entry(key);
                Ok(next)
            }
            IncrOutcome::Insert(next) => {
                self.insert_entry(SmallBytes::from_slice(key), Entry::new(Value::Int(next), None));
                Ok(next)
            }
        }
    }
}

enum IncrOutcome {
    Reweigh(i64),
    Insert(i64),
}

/// The bytes a string-class value materializes to on the cold shared
/// lane (the `Value::Int` re-pick case included — a canonical-integer
/// spill decodes back through the SET rules).
fn cold_string_bytes(v: &Value) -> Vec<u8> {
    match v {
        Value::Str(s) => s.as_slice().to_vec(),
        Value::ArcBulk(a) => a.as_ref().to_vec(),
        Value::Int(n) => {
            let mut tmp = itoa_i64_stack();
            format_i64_into(*n, &mut tmp).to_vec()
        }
        _ => unreachable!("string-tagged cold record decodes to a string class"),
    }
}
