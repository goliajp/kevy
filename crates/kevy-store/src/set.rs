//! `Store` set commands.

use crate::small_set::{AddResult, SmallSetData, promote};
use crate::value::{SetData, SmallBytes, Value, set_member_weight};
use crate::{Entry, Store, StoreError};
use std::sync::Arc;

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
            Some(Value::SmallSetInline(s)) => s.len() == 0,
            _ => false,
        };
        if empty {
            self.remove_entry(key);
        }
    }

    /// `SADD` — returns the count of newly-added members.
    pub fn sadd(&mut self, key: &[u8], members: &[Vec<u8>]) -> Result<usize, StoreError> {
        let slices: Vec<&[u8]> = members.iter().map(Vec::as_slice).collect();
        self.sadd_borrowed(key, &slices)
    }

    /// G4 (v1.25): borrowed-slice SADD — kills the per-member `Vec<u8>` alloc
    /// the dispatch layer used to do via `rest(args, 2)`. Behaviour identical
    /// to [`Self::sadd`]; mirrors valkey's `setTypeAdd(set, objectGetVal(
    /// c->argv[j]))` zero-copy hand-off (`t_set.c:611`).
    ///
    /// A.7 O5: takes the encoding-switch path. New key starts as
    /// `SmallSetInline` if the first member fits; subsequent inserts
    /// stay inline until `SmallSetData::try_add` returns `NoRoom`, at
    /// which point the set is promoted in-place to
    /// `Value::Set(Arc<KevySet>)` and the spilling member is re-inserted
    /// in the heap-backed variant.
    pub fn sadd_borrowed(
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
    /// `sadd_borrowed` can stay short. The split keeps each function
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
                if Arc::make_mut(s).insert(smb) {
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
    pub fn srem(&mut self, key: &[u8], members: &[Vec<u8>]) -> Result<usize, StoreError> {
        let slices: Vec<&[u8]> = members.iter().map(Vec::as_slice).collect();
        self.srem_borrowed(key, &slices)
    }

    /// G4 (v1.25): borrowed-slice SREM — see [`Self::sadd_borrowed`].
    pub fn srem_borrowed(
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
    pub fn spop(&mut self, key: &[u8], count: usize) -> Result<Vec<Vec<u8>>, StoreError> {
        let (out, delta) = {
            let mut o: Vec<Vec<u8>> = Vec::new();
            let mut d: i64 = 0;
            if let Some(v) = self.set_value_mut(key)? {
                match v {
                    Value::SmallSetInline(s) => {
                        let take: Vec<Vec<u8>> =
                            s.iter_slices().take(count).map(<[u8]>::to_vec).collect();
                        for m in &take {
                            s.try_remove(m.as_slice());
                        }
                        o = take;
                    }
                    Value::Set(s) => {
                        let set_mut = Arc::make_mut(s);
                        let take: Vec<Vec<u8>> = set_mut
                            .iter()
                            .take(count)
                            .map(kevy_bytes::SmallBytes::to_vec)
                            .collect();
                        for m in &take {
                            if set_mut.remove(m.as_slice()) {
                                d -= set_member_weight(&SmallBytes::from_slice(m)) as i64;
                            }
                        }
                        o = take;
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

    /// `SRANDMEMBER key count` — up to `count` arbitrary members, not removed.
    pub fn srandmember(&mut self, key: &[u8], count: usize) -> Result<Vec<Vec<u8>>, StoreError> {
        match self.live_entry(key) {
            None => Ok(Vec::new()),
            Some(e) => match &e.value {
                Value::Set(s) => Ok(s
                    .iter()
                    .take(count)
                    .map(kevy_bytes::SmallBytes::to_vec)
                    .collect()),
                Value::SmallSetInline(s) => {
                    Ok(s.iter_slices().take(count).map(<[u8]>::to_vec).collect())
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
/// `sadd_borrowed` route the weight delta correctly: inline adds carry
/// zero heap (no delta), heap adds carry the per-member byte budget,
/// already-present means no count + no delta.
enum SaddOutcome {
    AddedInline,
    AddedHeap(i64),
    AlreadyPresent,
}
