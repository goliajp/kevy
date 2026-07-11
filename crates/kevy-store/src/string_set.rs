//! `Store` SET-family write path: encoding pick (`Int` / `ArcBulk` /
//! inline `Str`), NX/XX guards, the single-probe `maxmemory == 0`
//! fast path, and the bio-drop hand-off of displaced values.
//! Split out of `string.rs` (GET family + INCR) to keep both under the
//! 500-LOC house cap.

use crate::value::{BULK_THRESHOLD, SmallBytes, Value};
use crate::{Entry, Store, deadline_at, now_ns};
use crate::util::parse_canonical_i64;
use std::sync::Arc;
use std::time::Duration;


/// L2 + L1: pick the optimal encoding for `bytes` at SET time:
/// 1. Canonical i64 ASCII → `Value::Int(n)` (smallest + INCR fast path)
/// 2. > [`BULK_THRESHOLD`] bytes → `Value::ArcBulk(Arc<[u8]>)` (lets the
///    > reactor reply path borrow the bytes for `writev` zero-copy GET)
/// 3. Else → `Value::Str(SmallBytes::from_slice(bytes))` (inline-cache-
///    line storage, beats Arc indirection for small values)
#[inline]
fn pick_value_for_set(bytes: &[u8]) -> Value {
    if let Some(n) = parse_canonical_i64(bytes) {
        return Value::Int(n);
    }
    if bytes.len() > BULK_THRESHOLD {
        return Value::ArcBulk(Arc::new(Box::<[u8]>::from(bytes)));
    }
    Value::Str(SmallBytes::from_slice(bytes))
}

#[inline]
pub(crate) fn pick_value_for_set_owned(bytes: Vec<u8>) -> Value {
    if let Some(n) = parse_canonical_i64(&bytes) {
        return Value::Int(n);
    }
    if bytes.len() > BULK_THRESHOLD {
        // `Arc::new(box)` is zero-copy when `len ==
        // capacity` (shrink-to-fit no-ops). See `Value::ArcBulk` doc.
        return Value::ArcBulk(Arc::new(bytes.into_boxed_slice()));
    }
    Value::Str(SmallBytes::from_vec(bytes))
}

impl Store {

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
        // Single-probe overwrite-SET fast path for default
        // `maxmemory == 0` (the bench and production-common case). Goes
        // through `kevy_map::RawEntryMut` so the
        // Occupied arm mutates the entry in place and returns owned
        // (delta, ttl_delta) — no escaping reference, no second probe.
        // Overwrite path drops from 2 probes (live_entry_mut: get+get_mut)
        // to 1 probe. New-key + expired-removed paths still pay the
        // insert_entry probe (same as before).
        if self.maxmemory == 0 {
            return self.set_value_no_evict(key, new_value, expire, nx, xx);
        }
        self.set_value_evict(key, new_value, expire, nx, xx)
    }

    /// Eviction path (maxmemory > 0) of [`Self::set_value`]: keeps the
    /// 2-probe shape so `live_entry_mut`'s touch_on_access bookkeeping runs.
    fn set_value_evict(
        &mut self,
        key: &[u8],
        new_value: Value,
        expire: Option<Duration>,
        nx: bool,
        xx: bool,
    ) -> bool {
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
                let (delta, ttl_delta, old) =
                    overwrite_in_place(e, new_value, expire_at, key_heap);
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
    /// `maxmemory == 0` fast path. Skips the `live_entry_mut`
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
        let expire_at = expire.map(|d| deadline_at(now_ns(), d));
        let key_heap = crate::key_heap_bytes_for(key);
        // Hold new_value behind Option so the multi-arm consumption
        // (overwrite arm vs insert-after-expired arm) is moved-once.
        let mut value_slot = Some(new_value);
        let outcome = self.set_probe_no_evict(key, &mut value_slot, expire_at, key_heap, nx, xx);
        // Phase 2: bookkeeping + maybe insert. Borrow on self.map is gone.
        let old_value: Option<Value> = match outcome {
            SetOutcome::Refused { drop_first } => {
                if let Some(old) = drop_first {
                    // No insert; the expired `Value` still needs
                    // to drop. Ship via the bio path on its way out.
                    self.maybe_offload_drop(old);
                }
                return false;
            }
            SetOutcome::Updated { delta, ttl_delta, old } => {
                self.apply_weight_delta(delta);
                self.adjust_expires(ttl_delta);
                Some(old)
            }
            SetOutcome::ExpiredThenInsert { old } => {
                let entry = Entry::new(value_slot.take().unwrap(), expire_at);
                self.insert_entry(SmallBytes::from_slice(key), entry);
                Some(old)
            }
            SetOutcome::NeedInsert => {
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

    /// Phase 1 of [`Self::set_value_no_evict`]: probe + decide. Four
    /// outcomes — overwrite-finished (delta + ttl_delta + the displaced
    /// old `Value` to maybe ship to the bio thread), an expired-removed
    /// `Entry` whose `Value` ditto needs offload, needs-insert with no
    /// prior value, or refused by an NX/XX guard.
    #[inline]
    fn set_probe_no_evict(
        &mut self,
        key: &[u8],
        value_slot: &mut Option<Value>,
        expire_at: Option<u64>,
        key_heap: u64,
        nx: bool,
        xx: bool,
    ) -> SetOutcome {
        use kevy_map::RawEntryMut;
        let (uc, cn) = (self.cached_clock, self.cached_ns);
        match self.map.raw_entry_mut(key) {
            RawEntryMut::Occupied(mut occ) => {
                // Decide via shared ref first so we don't touch the entry
                // (avoiding a needless cache-line dirtying) on the
                // is_expired+remove path.
                let expired = occ.get().is_expired(uc, cn);
                if expired {
                    let old = occ.remove();
                    // borrow on self.map released by remove(self).
                    self.note_expired_removed(&old);
                    if xx {
                        return SetOutcome::Refused { drop_first: Some(old.value) };
                    }
                    SetOutcome::ExpiredThenInsert { old: old.value }
                } else {
                    if nx {
                        return SetOutcome::Refused { drop_first: None };
                    }
                    // Take the old Value before overwriting
                    // so phase 2 can hand it to the bio thread instead
                    // of dropping inline (a large-value latency-tail
                    // amplifier).
                    let (delta, ttl_delta, old) = overwrite_in_place(
                        occ.get_mut(),
                        value_slot.take().unwrap(),
                        expire_at,
                        key_heap,
                    );
                    SetOutcome::Updated { delta, ttl_delta, old }
                }
            }
            RawEntryMut::Vacant(_) => {
                if xx {
                    return SetOutcome::Refused { drop_first: None };
                }
                SetOutcome::NeedInsert
            }
        }
    }

    /// Bookkeeping for an entry removed because the SET probe found it
    /// expired: subtract its weight, decrement the TTL gauge, bump the
    /// expired counter.
    #[inline]
    fn note_expired_removed(&mut self, old: &Entry) {
        self.used_memory = self
            .used_memory
            .saturating_sub(old.weight() + crate::value::ENTRY_OVERHEAD);
        if old.expire_at_ns.is_some() {
            self.adjust_expires(-1);
        }
        self.expired_keys_total = self.expired_keys_total.saturating_add(1);
    }
}


/// Phase-1 verdict of the SET probe (see [`Store::set_probe_no_evict`]).
enum SetOutcome {
    Updated { delta: i64, ttl_delta: i64, old: Value },
    ExpiredThenInsert { old: Value },
    NeedInsert,
    /// An NX/XX guard stopped the write; `drop_first` carries an
    /// expired-removed `Value` that still needs the bio-drop hand-off.
    Refused { drop_first: Option<Value> },
}

/// Overwrite one live entry's value + TTL in place; returns
/// `(weight_delta, ttl_delta, displaced_old_value)`. Shared by the
/// eviction-path SET ([`Store::set_value`]) and the `maxmemory == 0`
/// single-probe SET ([`Store::set_probe_no_evict`]) — the displaced
/// `Value` is returned so the caller can hand it to the
/// bio thread AFTER the keyspace borrow is released rather than
/// dropping inline (the Drop of a `Value::ArcBulk` over the heap-heavy
/// threshold amplifies the large-value SET latency tail).
#[inline]
fn overwrite_in_place(
    e: &mut Entry,
    new_value: Value,
    expire_at: Option<u64>,
    key_heap: u64,
) -> (i64, i64, Value) {
    let had_ttl = e.expire_at_ns.is_some();
    let old = std::mem::replace(&mut e.value, new_value);
    e.expire_at_ns = expire_at.and_then(crate::pack_deadline);
    let new_w = key_heap + e.value.weight();
    let delta = new_w as i64 - e.weight() as i64;
    let ttl_delta = i64::from(e.expire_at_ns.is_some()) - i64::from(had_ttl);
    e.set_weight(new_w);
    (delta, ttl_delta, old)
}
