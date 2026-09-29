//! [`KeyDir`] — row key → indexed value, for the indexes a view reads by
//! key. Every other index has none.
//!
//! One slot per row is 24 bytes — arena offset, key length, value length
//! and a 16-bit tag of the key's hash — and each row's key and encoded
//! value sit back to back in one arena, not in an allocation each. Linear
//! probing with backward-shift deletion leaves no tombstones; the arena is
//! rewritten when the bytes of removed rows outgrow the live ones.

use std::hash::Hasher;
use std::mem::size_of;

use kevy_hash::FxHasher;

use crate::value::IndexValue;

#[derive(Clone, Copy)]
struct Slot {
    off: u64,
    klen: u32,
    vlen: u32,
    tag: u16,
}

const EMPTY: Slot = Slot { off: 0, klen: u32::MAX, vlen: 0, tag: 0 };

impl Slot {
    fn is_empty(&self) -> bool {
        self.klen == u32::MAX
    }
}

/// Row key → the value its row is indexed under.
///
/// ```
/// use kevy_index::{IndexValue, Segment};
///
/// let mut s = Segment::new();
/// s.set_key_dir(true);
/// s.apply(b"user:1", None, Some(IndexValue::I64(30)));
/// let dir = s.key_dir().expect("asked for");
/// assert_eq!(dir.get(b"user:1"), Some(IndexValue::I64(30)));
/// assert_eq!((dir.len(), dir.get(b"user:2")), (1, None));
/// assert!(dir.approx_bytes() > 0 && !dir.is_empty());
/// ```
#[derive(Default)]
pub struct KeyDir {
    slots: Vec<Slot>,
    arena: Vec<u8>,
    len: usize,
    /// Arena bytes of rows since removed or rewritten.
    garbage: usize,
}

impl std::fmt::Debug for KeyDir {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeyDir").field("len", &self.len).finish_non_exhaustive()
    }
}

fn hash_key(key: &[u8]) -> u64 {
    let mut h = FxHasher::default();
    h.write(key);
    h.finish()
}

impl KeyDir {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Rows held.
    ///
    /// ```
    /// # use kevy_index::{IndexValue, Segment};
    /// let mut s = Segment::new();
    /// s.set_key_dir(true);
    /// s.apply(b"a", None, Some(IndexValue::I64(1)));
    /// assert_eq!(s.key_dir().map(|d| d.len()), Some(1));
    /// ```
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether no row is held.
    ///
    /// ```
    /// # use kevy_index::Segment;
    /// let mut s = Segment::new();
    /// s.set_key_dir(true);
    /// assert!(s.key_dir().is_some_and(|d| d.is_empty()));
    /// ```
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Heap bytes: every slot, and the arena.
    ///
    /// ```
    /// # use kevy_index::{IndexValue, Segment};
    /// let mut s = Segment::new();
    /// s.set_key_dir(true);
    /// s.apply(b"a", None, Some(IndexValue::I64(1)));
    /// assert!(s.key_dir().map_or(0, |d| d.approx_bytes()) >= 24);
    /// ```
    pub fn approx_bytes(&self) -> u64 {
        (self.slots.capacity() * size_of::<Slot>() + self.arena.capacity()) as u64
    }

    fn key(&self, s: &Slot) -> &[u8] {
        &self.arena[s.off as usize..s.off as usize + s.klen as usize]
    }

    /// The slot holding `key`, or the empty slot where it would go.
    fn find(&self, key: &[u8], h: u64) -> (usize, bool) {
        let mask = self.slots.len() - 1;
        let tag = (h >> 48) as u16;
        let mut i = h as usize & mask;
        loop {
            let s = &self.slots[i];
            if s.is_empty() {
                return (i, false);
            }
            if s.tag == tag && s.klen as usize == key.len() && self.key(s) == key {
                return (i, true);
            }
            i = (i + 1) & mask;
        }
    }

    /// The value `key`'s row is indexed under.
    ///
    /// ```
    /// # use kevy_index::{IndexValue, Segment};
    /// let mut s = Segment::new();
    /// s.set_key_dir(true);
    /// s.apply(b"a", None, Some(IndexValue::Str(b"tokyo".to_vec())));
    /// assert_eq!(s.key_dir().and_then(|d| d.get(b"a")), Some(IndexValue::Str(b"tokyo".to_vec())));
    /// ```
    pub fn get(&self, key: &[u8]) -> Option<IndexValue> {
        if self.slots.is_empty() {
            return None;
        }
        let (i, true) = self.find(key, hash_key(key)) else { return None };
        let s = &self.slots[i];
        let mut at = s.off as usize + s.klen as usize;
        IndexValue::decode(&self.arena, &mut at)
    }

    /// Set `key`'s value.
    pub(crate) fn put(&mut self, key: &[u8], v: &IndexValue) {
        if (self.len + 1) * 8 > self.slots.len() * 7 {
            self.rebuild((self.slots.len() * 2).max(16));
        }
        let h = hash_key(key);
        let (i, found) = self.find(key, h);
        let mut enc = Vec::with_capacity(16);
        v.encode(&mut enc);
        if found {
            let s = self.slots[i];
            if s.vlen as usize == enc.len() {
                let at = s.off as usize + s.klen as usize;
                self.arena[at..at + enc.len()].copy_from_slice(&enc);
                return;
            }
            self.garbage += s.klen as usize + s.vlen as usize;
        } else {
            self.len += 1;
        }
        let off = self.arena.len() as u64;
        self.arena.extend_from_slice(key);
        self.arena.extend_from_slice(&enc);
        let klen = u32::try_from(key.len()).expect("a key under 4 GiB");
        let vlen = u32::try_from(enc.len()).expect("a value under 4 GiB");
        self.slots[i] = Slot { off, klen, vlen, tag: (h >> 48) as u16 };
    }

    /// Drop `key`.
    pub(crate) fn remove(&mut self, key: &[u8]) {
        if self.slots.is_empty() {
            return;
        }
        let (mut i, found) = self.find(key, hash_key(key));
        if !found {
            return;
        }
        let gone = self.slots[i];
        let mask = self.slots.len() - 1;
        // shift back every later slot of the run that may sit at `i`
        let mut j = i;
        loop {
            j = (j + 1) & mask;
            let s = self.slots[j];
            if s.is_empty() {
                break;
            }
            let home = hash_key(self.key(&s)) as usize & mask;
            let stays = if i <= j { i < home && home <= j } else { i < home || home <= j };
            if !stays {
                self.slots[i] = s;
                i = j;
            }
        }
        self.slots[i] = EMPTY;
        self.len -= 1;
        self.garbage += gone.klen as usize + gone.vlen as usize;
        if self.garbage > 4096 && self.garbage * 2 > self.arena.len() {
            self.rebuild(self.slots.len());
        }
    }

    /// Re-lay every live row into `cap` slots and a fresh arena.
    fn rebuild(&mut self, cap: usize) {
        let old_slots = std::mem::replace(&mut self.slots, vec![EMPTY; cap]);
        let old_arena = std::mem::take(&mut self.arena);
        self.arena.reserve(old_arena.len() - self.garbage);
        self.garbage = 0;
        let mask = cap - 1;
        for s in old_slots.into_iter().filter(|s| !s.is_empty()) {
            let at = s.off as usize;
            let key = &old_arena[at..at + s.klen as usize];
            let mut i = hash_key(key) as usize & mask;
            while !self.slots[i].is_empty() {
                i = (i + 1) & mask;
            }
            let off = self.arena.len() as u64;
            self.arena.extend_from_slice(&old_arena[at..at + (s.klen + s.vlen) as usize]);
            self.slots[i] = Slot { off, ..s };
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    #[test]
    fn a_random_run_of_writes_agrees_with_a_map() {
        let (mut t, mut m) = (KeyDir::new(), HashMap::new());
        let mut x: u64 = 7;
        for step in 0..200_000u64 {
            x = x.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
            // a small key space, so inserts, updates and removes all collide
            let key = format!("k{}", (x >> 33) % 3_000).into_bytes();
            match (x >> 20) % 3 {
                0 => {
                    t.remove(&key);
                    m.remove(&key);
                }
                1 => {
                    let v = IndexValue::Str(vec![b's'; (step % 30) as usize]);
                    t.put(&key, &v);
                    m.insert(key.clone(), v);
                }
                _ => {
                    let v = IndexValue::I64(step as i64);
                    t.put(&key, &v);
                    m.insert(key.clone(), v);
                }
            }
            assert_eq!(t.len(), m.len());
        }
        for (k, v) in &m {
            assert_eq!(t.get(k).as_ref(), Some(v));
        }
        assert_eq!(t.get(b"absent"), None);
    }

    #[test]
    fn removed_keys_do_not_keep_their_bytes() {
        let mut t = KeyDir::new();
        for round in 0..20u32 {
            for i in 0..10_000u32 {
                t.put(format!("row:{round}:{i}").as_bytes(), &IndexValue::I64(1));
            }
            for i in 0..10_000u32 {
                t.remove(format!("row:{round}:{i}").as_bytes());
            }
        }
        t.put(b"last", &IndexValue::F64(0.5));
        assert!(t.arena.len() < 400_000, "the arena was rewritten: {} bytes", t.arena.len());
        assert_eq!(t.get(b"last"), Some(IndexValue::F64(0.5)));
    }

    #[test]
    fn a_row_costs_its_slot_share_its_key_and_its_value() {
        let mut t = KeyDir::new();
        for i in 0..100_000u32 {
            t.put(format!("user:{i}").as_bytes(), &IndexValue::I64(i.into()));
        }
        let per_row = t.approx_bytes() as f64 / 100_000.0;
        assert_eq!(size_of::<Slot>(), 24);
        // 24-byte slots at 3/8 to 7/8 load, a ten-byte key, a nine-byte value
        assert!(per_row < 24.0 / 0.375 + 30.0, "{per_row:.1} bytes a row");
    }
}
