//! `Store` string commands.

use crate::util::{
    fmt_num, format_i64_into, itoa_i64_stack, parse_canonical_i64, parse_f64, parse_i64,
};
use crate::value::{BULK_THRESHOLD, SmallBytes, Value};
use crate::{Entry, Store, StoreError, deadline_at, now_ns};
use std::borrow::Cow;
use std::sync::Arc;
use std::time::Duration;

/// L1 return shape for [`Store::get_for_reply`] — lets the reactor's reply
/// path choose between memcpy (`Bytes`) and writev zero-copy (`ArcBulk`)
/// off one keyspace lookup.
pub enum GetReply<'a> {
    /// Inline-encoded value — caller memcpys the bytes into its output Vec
    /// (small replies; encoding cost is tiny vs the RTT floor).
    Bytes(Cow<'a, [u8]>),
    /// L1 (2026-06-21): Arc-backed bulk. The reactor's reply path pushes
    /// the Arc into the conn's `output_arcs` so the next `writev` iovec
    /// list points DIRECTLY at the value bytes — skipping the per-GET
    /// memcpy that valkey's `tryAvoidBulkStrCopyToReply` likewise avoids.
    ArcBulk(Arc<[u8]>),
}

/// L2 + L1: pick the optimal encoding for `bytes` at SET time:
/// 1. Canonical i64 ASCII → `Value::Int(n)` (smallest + INCR fast path)
/// 2. > [`BULK_THRESHOLD`] bytes → `Value::ArcBulk(Arc<[u8]>)` (lets the
///    reactor reply path borrow the bytes for `writev` zero-copy GET)
/// 3. Else → `Value::Str(SmallBytes::from_slice(bytes))` (inline-cache-
///    line storage, beats Arc indirection for small values)
#[inline]
fn pick_value_for_set(bytes: &[u8]) -> Value {
    if let Some(n) = parse_canonical_i64(bytes) {
        return Value::Int(n);
    }
    if bytes.len() > BULK_THRESHOLD {
        return Value::ArcBulk(Arc::from(bytes));
    }
    Value::Str(SmallBytes::from_slice(bytes))
}

#[inline]
fn pick_value_for_set_owned(bytes: Vec<u8>) -> Value {
    if let Some(n) = parse_canonical_i64(&bytes) {
        return Value::Int(n);
    }
    if bytes.len() > BULK_THRESHOLD {
        // Vec<u8> → Box<[u8]> → Arc<[u8]> reuses the existing allocation
        // for the Box step; Arc::from(Box<[u8]>) wraps without copying.
        return Value::ArcBulk(Arc::from(bytes.into_boxed_slice()));
    }
    Value::Str(SmallBytes::from_vec(bytes))
}

impl Store {
    // ---- strings -------------------------------------------------------

    /// `SET` — overwrites any existing value/type. NX/XX guards; clears TTL.
    /// Takes an owned `Vec` so a >22 B value's allocation is adopted as-is
    /// (no copy). For callers holding a borrowed slice, prefer
    /// [`Self::set_slice`] — it skips the `to_vec` entirely for values that
    /// inline.
    pub fn set(
        &mut self,
        key: &[u8],
        value: Vec<u8>,
        expire: Option<Duration>,
        nx: bool,
        xx: bool,
    ) -> bool {
        self.set_value(key, pick_value_for_set_owned(value), expire, nx, xx)
    }

    /// [`Self::set`] for a borrowed value. Values ≤ 22 B store inline in the
    /// entry — zero allocator traffic, where `set(key, value.to_vec(), …)`
    /// paid a malloc for the `Vec` and a free when the inline copy dropped
    /// it (the dominant overwrite-SET pattern). Larger values pay the same
    /// single allocation either way.
    pub fn set_slice(
        &mut self,
        key: &[u8],
        value: &[u8],
        expire: Option<Duration>,
        nx: bool,
        xx: bool,
    ) -> bool {
        self.set_value(key, pick_value_for_set(value), expire, nx, xx)
    }

    fn set_value(
        &mut self,
        key: &[u8],
        new_value: Value,
        expire: Option<Duration>,
        nx: bool,
        xx: bool,
    ) -> bool {
        // F1 (v1.25): single-probe overwrite-SET fast path for default
        // `maxmemory == 0` (the bench and production-common case). Goes
        // through `kevy_map::RawEntryMut` (B.1, commit 1a9f9af) so the
        // Occupied arm mutates the entry in place and returns owned
        // (delta, ttl_delta) — no escaping reference, no second probe.
        // Overwrite path drops from 2 probes (live_entry_mut: get+get_mut)
        // to 1 probe. New-key + expired-removed paths still pay the
        // insert_entry probe (same as before).
        if self.maxmemory == 0 {
            return self.set_value_no_evict(key, new_value, expire, nx, xx);
        }
        // Eviction path (maxmemory > 0): keep the 2-probe shape so
        // `live_entry_mut`'s touch_on_access bookkeeping runs.
        let expire_at = expire.map(|d| deadline_at(now_ns(), d));
        let key_heap = crate::key_heap_bytes_for(key);
        #[allow(clippy::single_match_else)]
        // Phase 1: in-place overwrite or remember we need to insert.
        // The old value (if any) is taken via `mem::replace` so the bio
        // hand-off in phase 2 happens AFTER `self.live_entry_mut`'s
        // borrow on `self.map` is released — without splitting the
        // borrow we couldn't call `self.maybe_offload_drop`.
        let (outcome, old_value) = match self.live_entry_mut(key) {
            Some(e) => {
                if nx {
                    return false;
                }
                let had_ttl = e.expire_at_ns.is_some();
                // v1.25 A.3: capture the old Value so it can be shipped
                // to the bio thread post-borrow rather than dropped
                // inline (the Drop of a Value::ArcBulk over the heap
                // heavy threshold is the Axis I tail amplifier).
                let old = std::mem::replace(&mut e.value, new_value);
                e.expire_at_ns = expire_at.and_then(crate::pack_deadline);
                let new_w = key_heap + e.value.weight();
                let delta = new_w as i64 - e.weight() as i64;
                let ttl_delta = i64::from(e.expire_at_ns.is_some()) - i64::from(had_ttl);
                e.set_weight(new_w);
                (Ok((delta, ttl_delta)), Some(old))
            }
            None => {
                if xx {
                    return false;
                }
                (Err(Entry::new(new_value, expire_at)), None)
            }
        };
        match outcome {
            Ok((delta, ttl_delta)) => {
                self.apply_weight_delta(delta);
                self.adjust_expires(ttl_delta);
            }
            Err(entry) => {
                self.insert_entry(SmallBytes::from_slice(key), entry);
            }
        }
        // Phase 2: hand the old value off if heavy. Done last so the
        // critical mutation + bookkeeping commit before any (sub-µs in
        // steady state) channel send.
        if let Some(old) = old_value {
            self.maybe_offload_drop(old);
        }
        true
    }

    /// Single-probe overwrite-SET via `kevy_map::RawEntryMut` for the
    /// `maxmemory == 0` fast path (F1, v1.25). Skips the `live_entry_mut`
    /// 2-probe shape: Occupied arm mutates in place + returns owned
    /// (delta, ttl_delta); Expired arm removes via raw-entry handle + falls
    /// through to insert; Vacant arm goes to insert.
    fn set_value_no_evict(
        &mut self,
        key: &[u8],
        new_value: Value,
        expire: Option<Duration>,
        nx: bool,
        xx: bool,
    ) -> bool {
        use kevy_map::RawEntryMut;
        let expire_at = expire.map(|d| deadline_at(now_ns(), d));
        let key_heap = crate::key_heap_bytes_for(key);
        let (uc, cn) = (self.cached_clock, self.cached_ns);
        // Hold new_value behind Option so the multi-arm consumption
        // (overwrite arm vs insert-after-expired arm) is moved-once.
        let mut value_slot = Some(new_value);

        // Phase 1: probe + decide. Three outcomes — overwrite-finished
        // (delta + ttl_delta + the displaced old `Value` to maybe ship
        // to the bio thread), an expired-removed `Entry` whose `Value`
        // ditto needs offload, or needs-insert with no prior value.
        enum Outcome {
            Updated { delta: i64, ttl_delta: i64, old: Value },
            ExpiredThenInsert { old: Value },
            NeedInsert,
        }
        let outcome = match self.map.raw_entry_mut(key) {
            RawEntryMut::Occupied(mut occ) => {
                // Decide via shared ref first so we don't touch the entry
                // (avoiding a needless cache-line dirtying) on the
                // is_expired+remove path.
                let expired = occ.get().is_expired(uc, cn);
                if expired {
                    let old = occ.remove();
                    // borrow on self.map released by remove(self).
                    self.used_memory = self
                        .used_memory
                        .saturating_sub(old.weight() + crate::value::ENTRY_OVERHEAD);
                    if old.expire_at_ns.is_some() {
                        self.adjust_expires(-1);
                    }
                    self.expired_keys_total =
                        self.expired_keys_total.saturating_add(1);
                    if xx {
                        // No insert; the expired `Value` still needs
                        // to drop. Ship via the bio path on its way out.
                        self.maybe_offload_drop(old.value);
                        return false;
                    }
                    Outcome::ExpiredThenInsert { old: old.value }
                } else {
                    if nx {
                        return false;
                    }
                    let e = occ.get_mut();
                    let had_ttl = e.expire_at_ns.is_some();
                    // v1.25 A.3: take the old Value before overwriting
                    // so phase 2 can hand it to the bio thread instead
                    // of dropping inline (the Axis I tail amplifier).
                    let old = std::mem::replace(&mut e.value, value_slot.take().unwrap());
                    e.expire_at_ns = expire_at.and_then(crate::pack_deadline);
                    let new_w = key_heap + e.value.weight();
                    let delta = new_w as i64 - e.weight() as i64;
                    let ttl_delta = i64::from(e.expire_at_ns.is_some())
                        - i64::from(had_ttl);
                    e.set_weight(new_w);
                    Outcome::Updated { delta, ttl_delta, old }
                }
            }
            RawEntryMut::Vacant(_) => {
                if xx {
                    return false;
                }
                Outcome::NeedInsert
            }
        };

        // Phase 2: bookkeeping + maybe insert. Borrow on self.map is gone.
        let old_value: Option<Value> = match outcome {
            Outcome::Updated { delta, ttl_delta, old } => {
                self.apply_weight_delta(delta);
                self.adjust_expires(ttl_delta);
                Some(old)
            }
            Outcome::ExpiredThenInsert { old } => {
                let entry = Entry::new(value_slot.take().unwrap(), expire_at);
                self.insert_entry(SmallBytes::from_slice(key), entry);
                Some(old)
            }
            Outcome::NeedInsert => {
                let entry = Entry::new(value_slot.take().unwrap(), expire_at);
                self.insert_entry(SmallBytes::from_slice(key), entry);
                None
            }
        };
        // Phase 3: ship the displaced Value if any. Last so the keyspace
        // commit precedes any (sub-µs steady-state) channel send.
        if let Some(old) = old_value {
            self.maybe_offload_drop(old);
        }
        true
    }

    /// L1 (2026-06-21): GET variant that exposes the underlying encoding
    /// so the reactor's reply path can choose zero-copy
    /// (`Value::ArcBulk` → push the Arc to the conn's `output_arcs` for a
    /// writev iovec) vs memcpy (`Value::Str` / `Value::Int` → encode bytes
    /// into the conn's output Vec). ONE keyspace lookup; the variant tag
    /// chooses the encoding without a second probe.
    pub fn get_for_reply(&mut self, key: &[u8]) -> Result<Option<GetReply<'_>>, StoreError> {
        match self.live_entry(key) {
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

    /// A.6 (v1.25): fused GET-into-output. Skips the [`GetReply`] enum tag
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
        output_arcs: &mut Vec<(usize, Arc<[u8]>)>,
    ) -> Result<bool, StoreError> {
        match self.live_entry(key) {
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
    /// integer to ASCII without storing it. L2 (2026-06-21): `Value::Str`
    /// returns `Cow::Borrowed` (zero copy, same as before); `Value::Int`
    /// formats to a small owned `Vec<u8>` (up to 20 bytes for `i64::MIN`).
    pub fn get(&mut self, key: &[u8]) -> Result<Option<Cow<'_, [u8]>>, StoreError> {
        match self.live_entry(key) {
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
                _ => Err(StoreError::WrongType),
            },
        }
    }

    pub fn strlen(&mut self, key: &[u8]) -> Result<usize, StoreError> {
        Ok(self.get(key)?.map_or(0, |c| c.len()))
    }

    pub fn append(&mut self, key: &[u8], data: &[u8]) -> Result<usize, StoreError> {
        let outcome = match self.live_entry_mut(key) {
            Some(e) => match &mut e.value {
                Value::Str(v) => {
                    // SmallBytes is immutable; pop out, grow via Vec, re-wrap.
                    let mut owned = std::mem::take(v).into_vec();
                    owned.extend_from_slice(data);
                    let new_len = owned.len();
                    *v = SmallBytes::from_vec(owned);
                    AppendOutcome::Reweigh(new_len)
                }
                // L1: APPEND on Arc-backed bulk → materialise to a fresh
                // Vec (no other reader has refs to the old Arc post-replace),
                // append, then pick the new encoding via SET routing rules.
                Value::ArcBulk(a) => {
                    let mut owned: Vec<u8> = a.as_ref().to_vec();
                    owned.extend_from_slice(data);
                    let new_len = owned.len();
                    e.value = pick_value_for_set_owned(owned);
                    AppendOutcome::Reweigh(new_len)
                }
                _ => return Err(StoreError::WrongType),
            },
            None => AppendOutcome::Insert,
        };
        match outcome {
            AppendOutcome::Reweigh(new_len) => {
                self.reweigh_entry(key);
                Ok(new_len)
            }
            AppendOutcome::Insert => {
                self.insert_entry(
                    SmallBytes::from_slice(key),
                    Entry::new(Value::Str(SmallBytes::from_slice(data)), None),
                );
                Ok(data.len())
            }
        }
    }

    /// `INCRBY` family; preserves any TTL.
    ///
    /// L2 (2026-06-21, lessons from valkey OBJ_ENCODING_INT): the hot path
    /// matches `Value::Int(n)` and does the increment in place — no parse,
    /// no format, no allocation. The legacy `Value::Str` arm parses,
    /// increments, and **promotes** to `Value::Int(next)` so subsequent
    /// INCRs land on the fast path. Insert-new path also lands as `Int`.
    pub fn incr_by(&mut self, key: &[u8], delta: i64) -> Result<i64, StoreError> {
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
                self.insert_entry(
                    SmallBytes::from_slice(key),
                    Entry::new(Value::Int(next), None),
                );
                Ok(next)
            }
        }
    }

    /// `GETSET` — set to `val`, return the previous string (WRONGTYPE if the old
    /// value isn't a string). Clears any TTL, like SET.
    pub fn getset(&mut self, key: &[u8], val: Vec<u8>) -> Result<Option<Vec<u8>>, StoreError> {
        let old = match self.live_entry(key) {
            Some(e) => match &e.value {
                Value::Str(v) => Some(v.to_vec()),
                _ => return Err(StoreError::WrongType),
            },
            None => None,
        };
        self.insert_entry(
            SmallBytes::from_slice(key),
            Entry::new(Value::Str(SmallBytes::from_vec(val)), None),
        );
        Ok(old)
    }

    /// `GETDEL` — get then delete (WRONGTYPE if non-string).
    pub fn getdel(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>, StoreError> {
        let is_str = match self.live_entry(key) {
            None => return Ok(None),
            Some(e) => matches!(e.value, Value::Str(_)),
        };
        if !is_str {
            return Err(StoreError::WrongType);
        }
        match self.remove_entry(key) {
            Some(Entry {
                value: Value::Str(v),
                ..
            }) => Ok(Some(v.into_vec())),
            _ => Ok(None),
        }
    }

    /// `INCRBYFLOAT` — returns the new value formatted as Redis would. Preserves TTL.
    pub fn incr_by_float(&mut self, key: &[u8], delta: f64) -> Result<Vec<u8>, StoreError> {
        let outcome = if let Some(e) = self.live_entry_mut(key) { match &mut e.value {
            Value::Str(v) => {
                let next = parse_f64(v.as_slice()).ok_or(StoreError::NotFloat)? + delta;
                if !next.is_finite() {
                    return Err(StoreError::NotFloat);
                }
                let bytes = fmt_num(next);
                *v = SmallBytes::from_slice(&bytes);
                FloatOutcome::Reweigh(bytes)
            }
            _ => return Err(StoreError::WrongType),
        } } else {
            // Absent/expired ⇒ start from 0.0.
            if !delta.is_finite() {
                return Err(StoreError::NotFloat);
            }
            FloatOutcome::Insert(fmt_num(delta))
        };
        match outcome {
            FloatOutcome::Reweigh(bytes) => {
                self.reweigh_entry(key);
                Ok(bytes)
            }
            FloatOutcome::Insert(bytes) => {
                self.insert_entry(
                    SmallBytes::from_slice(key),
                    Entry::new(Value::Str(SmallBytes::from_slice(&bytes)), None),
                );
                Ok(bytes)
            }
        }
    }
}

enum AppendOutcome {
    Reweigh(usize),
    Insert,
}

enum IncrOutcome {
    Reweigh(i64),
    Insert(i64),
}

enum FloatOutcome {
    Reweigh(Vec<u8>),
    Insert(Vec<u8>),
}
