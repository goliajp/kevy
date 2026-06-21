//! `Store` set commands.

use crate::value::{SetData, SmallBytes, Value, set_member_weight};
use crate::{Entry, Store, StoreError};
use std::sync::Arc;

impl Store {
    // ---- sets ----------------------------------------------------------

    fn set_mut(&mut self, key: &[u8], create: bool) -> Result<Option<&mut SetData>, StoreError> {
        if self.live_entry_mut(key).is_none() {
            if !create {
                return Ok(None);
            }
            self.insert_entry(
                SmallBytes::from_slice(key),
                Entry::new(Value::Set(Arc::default()), None),
            );
        }
        match &mut self.map.get_mut(key).expect("present").value {
            Value::Set(s) => Ok(Some(Arc::make_mut(s))),
            _ => Err(StoreError::WrongType),
        }
    }

    fn set_ref(&mut self, key: &[u8]) -> Result<Option<&SetData>, StoreError> {
        match self.live_entry(key) {
            None => Ok(None),
            Some(e) => match &e.value {
                Value::Set(s) => Ok(Some(s.as_ref())),
                _ => Err(StoreError::WrongType),
            },
        }
    }

    fn drop_if_empty_set(&mut self, key: &[u8]) {
        let empty = matches!(self.map.get(key).map(|e| &e.value), Some(Value::Set(s)) if s.is_empty());
        if empty {
            self.remove_entry(key);
        }
    }

    /// `SADD` — returns the count of newly-added members.
    pub fn sadd(&mut self, key: &[u8], members: &[Vec<u8>]) -> Result<usize, StoreError> {
        let (added, delta) = {
            let s = self.set_mut(key, true)?.expect("created");
            let mut a = 0usize;
            let mut d: i64 = 0;
            for m in members {
                let smb = SmallBytes::from_slice(m);
                let w = set_member_weight(&smb) as i64;
                if s.insert(smb) {
                    a += 1;
                    d += w;
                }
            }
            (a, d)
        };
        self.account_delta(key, delta);
        Ok(added)
    }

    /// G4 (v1.25): borrowed-slice SADD — kills the per-member `Vec<u8>` alloc
    /// the dispatch layer used to do via `rest(args, 2)`. Behaviour identical
    /// to [`Self::sadd`]; mirrors valkey's `setTypeAdd(set, objectGetVal(
    /// c->argv[j]))` zero-copy hand-off (`t_set.c:611`).
    pub fn sadd_borrowed(
        &mut self,
        key: &[u8],
        members: &[&[u8]],
    ) -> Result<usize, StoreError> {
        let (added, delta) = {
            let s = self.set_mut(key, true)?.expect("created");
            let mut a = 0usize;
            let mut d: i64 = 0;
            for m in members {
                let smb = SmallBytes::from_slice(m);
                let w = set_member_weight(&smb) as i64;
                if s.insert(smb) {
                    a += 1;
                    d += w;
                }
            }
            (a, d)
        };
        self.account_delta(key, delta);
        Ok(added)
    }

    /// `SREM` — returns the count removed (deleting an emptied key).
    pub fn srem(&mut self, key: &[u8], members: &[Vec<u8>]) -> Result<usize, StoreError> {
        let (removed, delta) = {
            let mut r = 0usize;
            let mut d: i64 = 0;
            if let Some(s) = self.set_mut(key, false)? {
                for m in members {
                    if s.remove(m.as_slice()) {
                        r += 1;
                        d -= set_member_weight(&SmallBytes::from_slice(m)) as i64;
                    }
                }
            }
            (r, d)
        };
        self.account_delta(key, delta);
        self.drop_if_empty_set(key);
        Ok(removed)
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
            if let Some(s) = self.set_mut(key, false)? {
                for m in members {
                    if s.remove(*m) {
                        r += 1;
                        d -= set_member_weight(&SmallBytes::from_slice(m)) as i64;
                    }
                }
            }
            (r, d)
        };
        self.account_delta(key, delta);
        self.drop_if_empty_set(key);
        Ok(removed)
    }

    pub fn sismember(&mut self, key: &[u8], member: &[u8]) -> Result<bool, StoreError> {
        Ok(self.set_ref(key)?.is_some_and(|s| s.contains(member)))
    }

    pub fn scard(&mut self, key: &[u8]) -> Result<usize, StoreError> {
        Ok(self.set_ref(key)?.map_or(0, kevy_map::KevySet::len))
    }

    pub fn smembers(&mut self, key: &[u8]) -> Result<Vec<Vec<u8>>, StoreError> {
        Ok(self
            .set_ref(key)?
            .map_or(Vec::new(), |s| s.iter().map(kevy_bytes::SmallBytes::to_vec).collect()))
    }

    /// `SPOP key count` — remove and return up to `count` arbitrary members.
    pub fn spop(&mut self, key: &[u8], count: usize) -> Result<Vec<Vec<u8>>, StoreError> {
        let (out, delta) = {
            let mut o: Vec<Vec<u8>> = Vec::new();
            let mut d: i64 = 0;
            if let Some(s) = self.set_mut(key, false)? {
                let take: Vec<Vec<u8>> = s.iter().take(count).map(kevy_bytes::SmallBytes::to_vec).collect();
                for m in &take {
                    if s.remove(m.as_slice()) {
                        d -= set_member_weight(&SmallBytes::from_slice(m)) as i64;
                    }
                }
                o = take;
            }
            (o, d)
        };
        self.account_delta(key, delta);
        self.drop_if_empty_set(key);
        Ok(out)
    }

    /// `SRANDMEMBER key count` — up to `count` arbitrary members, not removed.
    pub fn srandmember(&mut self, key: &[u8], count: usize) -> Result<Vec<Vec<u8>>, StoreError> {
        Ok(self
            .set_ref(key)?
            .map_or(Vec::new(), |s| {
                s.iter().take(count).map(kevy_bytes::SmallBytes::to_vec).collect()
            }))
    }

    /// Snapshot of a set's members for cross-shard algebra (SINTER/etc.).
    pub fn set_snapshot(&mut self, key: &[u8]) -> Result<Vec<Vec<u8>>, StoreError> {
        self.smembers(key)
    }
}
