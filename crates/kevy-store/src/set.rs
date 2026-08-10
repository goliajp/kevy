//! `Store` set commands.

#[cfg(not(feature = "std"))]
use crate::nostd_prelude::*;
use crate::small_set::{AddResult, SmallSetData, promote};
use crate::value::{SetData, SmallBytes, Value, set_member_weight};
use crate::{Entry, Store, StoreError};
use alloc::sync::Arc;

impl Store {
    // ---- sets ----------------------------------------------------------

    /// Borrow the value at `key` for mutation. Returns `None` if the key
    /// is absent (and `create == false`) or if the entry exists but is a
    /// non-set type (returns `WrongType`). On `create == true` for a
    /// missing key, **does not** materialise an entry — the caller
    /// decides between `SmallSetInline` and `Set` based on the first
    /// member, so we don't pre-allocate an empty `Arc<KevySet>` only to
    /// discard it.
    fn set_value_mut(
        &mut self,
        key: &[u8],
    ) -> Result<Option<&mut Value>, StoreError> {
        match self.live_entry_mut(key) {
            None => Ok(None),
            Some(e) => match &e.value {
                Value::Set(_) | Value::SmallSetInline(_) => Ok(Some(&mut e.value)),
                _ => Err(StoreError::WrongType),
            },
        }
    }

    fn drop_if_empty_set(&mut self, key: &[u8]) {
        let empty = match self.map.get(key).map(|e| &e.value) {
            Some(Value::Set(s)) => s.is_empty(),
            Some(Value::SmallSetInline(s)) => s.is_empty(),
            _ => false,
        };
        if empty {
            self.remove_entry(key);
        }
    }

    /// `SADD` — returns the count of newly-added members. Borrowed argv:
    /// no per-member allocation on the insert path.
    ///
    /// Takes the encoding-switch path: a new key starts as
    /// `SmallSetInline` if the first member fits; subsequent inserts
    /// stay inline until `SmallSetData::try_add` returns `NoRoom`, at
    /// which point the set is promoted in-place to
    /// `Value::Set(Arc<KevySet>)` and the spilling member is re-inserted
    /// in the heap-backed variant.
    pub fn sadd(
        &mut self,
        key: &[u8],
        members: &[&[u8]],
    ) -> Result<usize, StoreError> {
        if members.is_empty() {
            return Ok(0);
        }
        let mut added = 0usize;
        let mut delta: i64 = 0;
        for m in members {
            match self.sadd_one(key, m)? {
                SaddOutcome::AddedInline => {
                    added += 1;
                    // SmallSetInline carries zero heap (see
                    // `Value::weight` arm). The 1-byte length prefix +
                    // member bytes live inside the Value enum body.
                }
                SaddOutcome::AddedHeap(w) => {
                    added += 1;
                    delta += w;
                }
                SaddOutcome::AlreadyPresent => {}
            }
        }
        self.account_delta(key, delta);
        Ok(added)
    }

    /// Insert one member; encapsulates the encoding-switch decision so
    /// `sadd` can stay short. The split keeps each function
    /// under the 50-LOC house rule.
    fn sadd_one(&mut self, key: &[u8], m: &[u8]) -> Result<SaddOutcome, StoreError> {
        // Missing key — pick the encoding by member size.
        if self.set_value_mut(key)?.is_none() {
            return Ok(self.sadd_create(key, m));
        }
        let v = self.set_value_mut(key)?.expect("present and a set type");
        match v {
            Value::SmallSetInline(s) => match s.try_add(m) {
                AddResult::Added => Ok(SaddOutcome::AddedInline),
                AddResult::AlreadyPresent => Ok(SaddOutcome::AlreadyPresent),
                AddResult::NoRoom => {
                    // Upgrade in place: promote inline to KevySet, then
                    // insert the spilling member into the heap set.
                    let mut promoted = promote(s);
                    let smb = SmallBytes::from_slice(m);
                    let w = set_member_weight(&smb) as i64;
                    let inserted = promoted.insert(smb);
                    debug_assert!(inserted, "promote re-inserts existing inline");
                    *v = Value::Set(Arc::new(promoted));
                    // The upgrade itself adds the heap weight of the
                    // promoted set; `reweigh_entry` recomputes from
                    // scratch so we don't have to track per-member
                    // deltas separately for the inline→heap step.
                    self.reweigh_entry(key);
                    if inserted {
                        Ok(SaddOutcome::AddedHeap(w))
                    } else {
                        Ok(SaddOutcome::AlreadyPresent)
                    }
                }
            },
            Value::Set(s) => {
                let smb = SmallBytes::from_slice(m);
                let w = set_member_weight(&smb) as i64;
                let diag_shared = Arc::strong_count(s) > 1;
                let diag_t0 = std::time::Instant::now();
                let s = Arc::make_mut(s);
                if diag_shared {
                    let ms = diag_t0.elapsed().as_millis();
                    if ms >= 10 {
                        eprintln!("kevy-diag: COW set clone took {ms} ms ({} members)", s.len());
                    }
                }
                if s.insert(smb) {
                    Ok(SaddOutcome::AddedHeap(w))
                } else {
                    Ok(SaddOutcome::AlreadyPresent)
                }
            }
            _ => Err(StoreError::WrongType),
        }
    }

    /// Create a fresh entry for `key` holding one member. Picks
    /// `SmallSetInline` when the member fits the inline budget, falls
    /// back to `Value::Set(Arc<KevySet>)` otherwise.
    ///
    /// Returns [`SaddOutcome::AddedInline`] either way — the
    /// `insert_entry` call has already accounted for the member's
    /// weight via `value.weight()`, so the caller MUST NOT also apply
    /// a delta. `AddedInline` carries zero delta in the caller, which
    /// is exactly the right shape for the heap-backed branch too.
    fn sadd_create(&mut self, key: &[u8], m: &[u8]) -> SaddOutcome {
        if let Some(inline) = SmallSetData::with_one(m) {
            self.insert_entry(
                SmallBytes::from_slice(key),
                Entry::new(Value::SmallSetInline(inline), None),
            );
        } else {
            let smb = SmallBytes::from_slice(m);
            let mut s = SetData::with_capacity(1);
            s.insert(smb);
            self.insert_entry(
                SmallBytes::from_slice(key),
                Entry::new(Value::Set(Arc::new(s)), None),
            );
        }
        SaddOutcome::AddedInline
    }

    /// `SREM` — returns the count removed (deleting an emptied key).
    pub fn srem(
        &mut self,
        key: &[u8],
        members: &[&[u8]],
    ) -> Result<usize, StoreError> {
        let (removed, delta) = {
            let mut r = 0usize;
            let mut d: i64 = 0;
            if let Some(v) = self.set_value_mut(key)? {
                match v {
                    Value::SmallSetInline(s) => {
                        for m in members {
                            if s.try_remove(m) {
                                r += 1;
                                // Inline removal is zero-heap; no delta.
                            }
                        }
                    }
                    Value::Set(s) => {
                        let set_mut = Arc::make_mut(s);
                        for m in members {
                            if set_mut.remove(*m) {
                                r += 1;
                                d -= set_member_weight(&SmallBytes::from_slice(m)) as i64;
                            }
                        }
                    }
                    _ => return Err(StoreError::WrongType),
                }
            }
            (r, d)
        };
        self.account_delta(key, delta);
        self.drop_if_empty_set(key);
        Ok(removed)
    }

    pub fn sismember(&mut self, key: &[u8], member: &[u8]) -> Result<bool, StoreError> {
        match self.live_entry(key) {
            None => Ok(false),
            Some(e) => match &e.value {
                Value::Set(s) => Ok(s.contains(member)),
                Value::SmallSetInline(s) => Ok(s.contains(member)),
                _ => Err(StoreError::WrongType),
            },
        }
    }

    pub fn scard(&mut self, key: &[u8]) -> Result<usize, StoreError> {
        match self.live_entry(key) {
            None => Ok(0),
            Some(e) => match &e.value {
                Value::Set(s) => Ok(s.len()),
                Value::SmallSetInline(s) => Ok(s.len()),
                _ => Err(StoreError::WrongType),
            },
        }
    }

    pub fn smembers(&mut self, key: &[u8]) -> Result<Vec<Vec<u8>>, StoreError> {
        match self.live_entry(key) {
            None => Ok(Vec::new()),
            Some(e) => match &e.value {
                Value::Set(s) => {
                    Ok(s.iter().map(kevy_bytes::SmallBytes::to_vec).collect())
                }
                Value::SmallSetInline(s) => {
                    Ok(s.iter_slices().map(<[u8]>::to_vec).collect())
                }
                _ => Err(StoreError::WrongType),
            },
        }
    }

    /// `SPOP key count` — remove and return up to `count` arbitrary members.
    ///
    /// Arbitrary means arbitrary. This used to be `iter().take(count)`, which on
    /// a fixed set returns the same members in the same order forever — so
    /// draining a queue, sampling a population and assigning a bucket all
    /// silently did the wrong thing while looking like they worked.
    ///
    /// Each member is drawn by starting at a random slot and taking the first
    /// occupied one, which is O(1) expected — the same thing Redis's
    /// `dictGetRandomKey` does, with the same slight bias towards members that
    /// follow a run of empty slots.
    pub fn spop(&mut self, key: &[u8], count: usize) -> Result<Vec<Vec<u8>>, StoreError> {
        let mut draws: Vec<u64> = (0..count).map(|_| self.rng.next_u64()).collect();
        let (out, delta) = {
            let mut o: Vec<Vec<u8>> = Vec::new();
            let mut d: i64 = 0;
            if let Some(v) = self.set_value_mut(key)? {
                match v {
                    Value::SmallSetInline(s) => {
                        // At most a couple of members inline; collect, shuffle,
                        // take. O(n) on n <= 2 is O(1).
                        let mut all: Vec<Vec<u8>> = s.iter_slices().map(<[u8]>::to_vec).collect();
                        let k = shuffle_prefix(&mut all, count, &mut draws);
                        all.truncate(k);
                        for m in &all {
                            s.try_remove(m.as_slice());
                        }
                        o = all;
                    }
                    Value::Set(s) => {
                        let set_mut = Arc::make_mut(s);
                        for slot in draws.iter().take(count) {
                            if set_mut.is_empty() {
                                break;
                            }
                            let Some(m) = set_mut
                                .iter_from_slot(*slot as usize)
                                .next()
                                .map(kevy_bytes::SmallBytes::to_vec)
                            else {
                                break;
                            };
                            if set_mut.remove(m.as_slice()) {
                                d -= set_member_weight(&SmallBytes::from_slice(&m)) as i64;
                            }
                            o.push(m);
                        }
                    }
                    _ => return Err(StoreError::WrongType),
                }
            }
            (o, d)
        };
        self.account_delta(key, delta);
        self.drop_if_empty_set(key);
        Ok(out)
    }

    /// `SRANDMEMBER key count` — up to `count` DISTINCT arbitrary members, not
    /// removed. See [`Store::srandmember_with_repeats`] for the negative-count
    /// form.
    ///
    /// Two regimes, as Redis has: when `count` is a small fraction of the set,
    /// probe random slots and reject duplicates — O(count) expected. When it is
    /// most of the set, rejection would thrash, so copy the members out and
    /// shuffle a prefix instead — O(n), which you were paying anyway to return
    /// that many.
    pub fn srandmember(&mut self, key: &[u8], count: usize) -> Result<Vec<Vec<u8>>, StoreError> {
        let mut draws: Vec<u64> = (0..count.saturating_mul(3).max(8))
            .map(|_| self.rng.next_u64())
            .collect();
        match self.live_entry(key) {
            None => Ok(Vec::new()),
            Some(e) => match &e.value {
                Value::SmallSetInline(s) => {
                    let mut all: Vec<Vec<u8>> = s.iter_slices().map(<[u8]>::to_vec).collect();
                    let k = shuffle_prefix(&mut all, count, &mut draws);
                    all.truncate(k);
                    Ok(all)
                }
                Value::Set(s) => {
                    let n = s.len();
                    if count >= n {
                        return Ok(s.iter().map(kevy_bytes::SmallBytes::to_vec).collect());
                    }
                    if count * 4 >= n {
                        // Wanting most of the set: copying beats rejecting.
                        let mut all: Vec<Vec<u8>> =
                            s.iter().map(kevy_bytes::SmallBytes::to_vec).collect();
                        let k = shuffle_prefix(&mut all, count, &mut draws);
                        all.truncate(k);
                        return Ok(all);
                    }
                    let mut out: Vec<Vec<u8>> = Vec::with_capacity(count);
                    for slot in &draws {
                        if out.len() == count {
                            break;
                        }
                        if let Some(m) = s
                            .iter_from_slot(*slot as usize)
                            .next()
                            .map(kevy_bytes::SmallBytes::to_vec)
                            && !out.contains(&m)
                        {
                            out.push(m);
                        }
                    }
                    Ok(out)
                }
                _ => Err(StoreError::WrongType),
            },
        }
    }

    /// `SRANDMEMBER key -count` — exactly `count` members, WITH repetition.
    ///
    /// Redis has had this form since 2.6 and kevy rejected it outright, which is
    /// the shape a caller uses to sample with replacement.
    pub fn srandmember_with_repeats(
        &mut self,
        key: &[u8],
        count: usize,
    ) -> Result<Vec<Vec<u8>>, StoreError> {
        let draws: Vec<u64> = (0..count).map(|_| self.rng.next_u64()).collect();
        match self.live_entry(key) {
            None => Ok(Vec::new()),
            Some(e) => match &e.value {
                Value::SmallSetInline(s) => {
                    let all: Vec<Vec<u8>> = s.iter_slices().map(<[u8]>::to_vec).collect();
                    if all.is_empty() {
                        return Ok(Vec::new());
                    }
                    Ok(draws
                        .iter()
                        .map(|d| all[(*d as usize) % all.len()].clone())
                        .collect())
                }
                Value::Set(s) => {
                    if s.is_empty() {
                        return Ok(Vec::new());
                    }
                    Ok(draws
                        .iter()
                        .filter_map(|d| {
                            s.iter_from_slot(*d as usize)
                                .next()
                                .map(kevy_bytes::SmallBytes::to_vec)
                        })
                        .collect())
                }
                _ => Err(StoreError::WrongType),
            },
        }
    }

    /// Snapshot of a set's members for cross-shard algebra (SINTER/etc.).
    pub fn set_snapshot(&mut self, key: &[u8]) -> Result<Vec<Vec<u8>>, StoreError> {
        self.smembers(key)
    }
}

/// Per-member result for the inner [`Store::sadd_one`] step. Lets
/// `sadd` route the weight delta correctly: inline adds carry
/// zero heap (no delta), heap adds carry the per-member byte budget,
/// already-present means no count + no delta.
enum SaddOutcome {
    AddedInline,
    AddedHeap(i64),
    AlreadyPresent,
}

/// Fisher-Yates over the first `k` positions, using pre-drawn randomness.
///
/// The draws are taken from the store's RNG BEFORE the value is borrowed —
/// `set_value_mut` holds `&mut self`, so `self.rng` is unreachable inside. Doing
/// the draws up front is cheaper than the alternatives and keeps the borrow
/// checker out of the way.
fn shuffle_prefix<T>(items: &mut [T], k: usize, draws: &mut Vec<u64>) -> usize {
    let n = items.len();
    let k = k.min(n);
    for i in 0..k {
        let span = (n - i) as u64;
        let d = draws.pop().unwrap_or(i as u64);
        items.swap(i, i + crate::rng::below(d, span) as usize);
    }
    k
}
