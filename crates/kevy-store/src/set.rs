//! `Store` set commands.

use crate::value::*;
use crate::{Entry, Store, StoreError};

impl Store {
    // ---- sets ----------------------------------------------------------

    fn set_mut(&mut self, key: &[u8], create: bool) -> Result<Option<&mut SetData>, StoreError> {
        if self.live_entry_mut(key).is_none() {
            if !create {
                return Ok(None);
            }
            self.map.insert(
                SmallBytes::from_slice(key),
                Entry {
                    value: Value::Set(Box::default()),
                    expire_at: None,
                },
            );
        }
        match &mut self.map.get_mut(key).expect("present").value {
            Value::Set(s) => Ok(Some(s)),
            _ => Err(StoreError::WrongType),
        }
    }

    fn set_ref(&mut self, key: &[u8]) -> Result<Option<&SetData>, StoreError> {
        match self.live_entry(key) {
            None => Ok(None),
            Some(e) => match &e.value {
                Value::Set(s) => Ok(Some(s)),
                _ => Err(StoreError::WrongType),
            },
        }
    }

    fn drop_if_empty_set(&mut self, key: &[u8]) {
        if let Some(Value::Set(s)) = self.map.get(key).map(|e| &e.value)
            && s.is_empty()
        {
            self.map.remove(key);
        }
    }

    /// `SADD` — returns the count of newly-added members.
    pub fn sadd(&mut self, key: &[u8], members: &[Vec<u8>]) -> Result<usize, StoreError> {
        let s = self.set_mut(key, true)?.expect("created");
        Ok(members
            .iter()
            .filter(|m| s.insert(SmallBytes::from_slice(m)))
            .count())
    }

    /// `SREM` — returns the count removed (deleting an emptied key).
    pub fn srem(&mut self, key: &[u8], members: &[Vec<u8>]) -> Result<usize, StoreError> {
        let removed = match self.set_mut(key, false)? {
            None => 0,
            Some(s) => members.iter().filter(|m| s.remove(m.as_slice())).count(),
        };
        self.drop_if_empty_set(key);
        Ok(removed)
    }

    pub fn sismember(&mut self, key: &[u8], member: &[u8]) -> Result<bool, StoreError> {
        Ok(self.set_ref(key)?.is_some_and(|s| s.contains(member)))
    }

    pub fn scard(&mut self, key: &[u8]) -> Result<usize, StoreError> {
        Ok(self.set_ref(key)?.map_or(0, |s| s.len()))
    }

    pub fn smembers(&mut self, key: &[u8]) -> Result<Vec<Vec<u8>>, StoreError> {
        Ok(self
            .set_ref(key)?
            .map_or(Vec::new(), |s| s.iter().map(|m| m.to_vec()).collect()))
    }

    /// `SPOP key count` — remove and return up to `count` arbitrary members.
    pub fn spop(&mut self, key: &[u8], count: usize) -> Result<Vec<Vec<u8>>, StoreError> {
        let out = match self.set_mut(key, false)? {
            None => Vec::new(),
            Some(s) => {
                let take: Vec<Vec<u8>> = s.iter().take(count).map(|m| m.to_vec()).collect();
                for m in &take {
                    s.remove(m.as_slice());
                }
                take
            }
        };
        self.drop_if_empty_set(key);
        Ok(out)
    }

    /// `SRANDMEMBER key count` — up to `count` arbitrary members, not removed.
    pub fn srandmember(&mut self, key: &[u8], count: usize) -> Result<Vec<Vec<u8>>, StoreError> {
        Ok(self
            .set_ref(key)?
            .map_or(Vec::new(), |s| {
                s.iter().take(count).map(|m| m.to_vec()).collect()
            }))
    }

    /// Snapshot of a set's members for cross-shard algebra (SINTER/etc.).
    pub fn set_snapshot(&mut self, key: &[u8]) -> Result<Vec<Vec<u8>>, StoreError> {
        self.smembers(key)
    }
}
