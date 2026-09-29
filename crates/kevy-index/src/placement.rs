//! Where each row's global-index entry went, kept on the row's shard: key
//! → (partition, entry hash). A write compares the row's new entry with
//! this to send nothing, one upsert, or a delete and an upsert.
//!
//! One slot per row is 24 bytes — arena offset, key length, partition, a
//! 16-bit tag of the key's hash, and the entry hash — and the key bytes sit
//! back to back in one arena, not in an allocation each. Linear probing
//! with backward-shift deletion leaves no tombstones; the arena is rewritten
//! when the bytes of removed keys outgrow the live ones.

use std::hash::Hasher;

use kevy_hash::FxHasher;

#[derive(Clone, Copy)]
struct Slot {
    off: u64,
    len: u32,
    part: u16,
    tag: u16,
    hash: u64,
}

const EMPTY: Slot = Slot { off: 0, len: u32::MAX, part: 0, tag: 0, hash: 0 };

impl Slot {
    fn is_empty(&self) -> bool {
        self.len == u32::MAX
    }
}

/// Row key → `(partition, entry hash)`.
///
/// ```
/// use kevy_index::PlacementTable;
///
/// let mut t = PlacementTable::new();
/// assert!(t.insert(b"user:1", 2, 0xfeed));
/// assert!(!t.insert(b"user:1", 3, 0xbeef), "a second insert updates");
/// assert_eq!(t.get(b"user:1"), Some((3, 0xbeef)));
/// assert_eq!(t.remove(b"user:1"), Some((3, 0xbeef)));
/// assert!(t.is_empty());
///
/// t.insert(b"user:2", 0, 1);
/// t.insert(b"user:3", 1, 2);
/// assert_eq!(t.len(), 2);
/// assert!(t.approx_bytes() > 0);
/// t.clear();
/// assert_eq!((t.len(), t.get(b"user:2")), (0, None));
/// ```
#[derive(Default)]
pub struct PlacementTable {
    slots: Vec<Slot>,
    arena: Vec<u8>,
    len: usize,
    /// Arena bytes of keys since removed.
    garbage: usize,
}

impl std::fmt::Debug for PlacementTable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlacementTable").field("len", &self.len).finish_non_exhaustive()
    }
}

fn hash_key(key: &[u8]) -> u64 {
    let mut h = FxHasher::default();
    h.write(key);
    h.finish()
}

impl PlacementTable {
    /// An empty table; it allocates on the first insert.
    pub fn new() -> Self {
        Self::default()
    }

    /// Rows held.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether no row is held.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Heap bytes: every slot, and the arena.
    pub fn approx_bytes(&self) -> u64 {
        (self.slots.capacity() * size_of::<Slot>() + self.arena.capacity()) as u64
    }

    fn key(&self, s: &Slot) -> &[u8] {
        &self.arena[s.off as usize..s.off as usize + s.len as usize]
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
            if s.tag == tag && s.len as usize == key.len() && self.key(s) == key {
                return (i, true);
            }
            i = (i + 1) & mask;
        }
    }

    /// `key`'s partition and entry hash.
    pub fn get(&self, key: &[u8]) -> Option<(u16, u64)> {
        if self.slots.is_empty() {
            return None;
        }
        match self.find(key, hash_key(key)) {
            (i, true) => Some((self.slots[i].part, self.slots[i].hash)),
            _ => None,
        }
    }

    /// Set `key`'s placement; `true` when the key is new.
    pub fn insert(&mut self, key: &[u8], part: u16, hash: u64) -> bool {
        if (self.len + 1) * 8 > self.slots.len() * 7 {
            self.rebuild((self.slots.len() * 2).max(16));
        }
        let h = hash_key(key);
        let (i, found) = self.find(key, h);
        if found {
            let s = &mut self.slots[i];
            (s.part, s.hash) = (part, hash);
            return false;
        }
        let off = self.arena.len() as u64;
        self.arena.extend_from_slice(key);
        let len = u32::try_from(key.len()).expect("a key under 4 GiB");
        self.slots[i] = Slot { off, len, part, tag: (h >> 48) as u16, hash };
        self.len += 1;
        true
    }

    /// Drop `key`, returning where it was.
    pub fn remove(&mut self, key: &[u8]) -> Option<(u16, u64)> {
        if self.slots.is_empty() {
            return None;
        }
        let (mut i, found) = self.find(key, hash_key(key));
        if !found {
            return None;
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
        self.garbage += gone.len as usize;
        if self.garbage > 4096 && self.garbage * 2 > self.arena.len() {
            self.rebuild(self.slots.len());
        }
        Some((gone.part, gone.hash))
    }

    /// Forget every row.
    pub fn clear(&mut self) {
        *self = Self::new();
    }

    /// Re-lay every live row into `cap` slots and a fresh arena.
    fn rebuild(&mut self, cap: usize) {
        let old_slots = std::mem::replace(&mut self.slots, vec![EMPTY; cap]);
        let old_arena = std::mem::take(&mut self.arena);
        self.arena.reserve(old_arena.len() - self.garbage);
        self.garbage = 0;
        let mask = cap - 1;
        for s in old_slots.into_iter().filter(|s| !s.is_empty()) {
            let key = &old_arena[s.off as usize..s.off as usize + s.len as usize];
            let mut i = hash_key(key) as usize & mask;
            while !self.slots[i].is_empty() {
                i = (i + 1) & mask;
            }
            let off = self.arena.len() as u64;
            self.arena.extend_from_slice(key);
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
        let (mut t, mut m) = (PlacementTable::new(), HashMap::new());
        let mut x: u64 = 7;
        for step in 0..200_000u64 {
            x = x.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
            // a small key space, so inserts, updates and removes all collide
            let key = format!("k{}", (x >> 33) % 3_000).into_bytes();
            match (x >> 20) % 3 {
                0 => assert_eq!(t.remove(&key), m.remove(&key), "step {step}"),
                _ => {
                    let v = ((x >> 8) as u16, step);
                    assert_eq!(t.insert(&key, v.0, v.1), m.insert(key.clone(), v).is_none());
                }
            }
            assert_eq!(t.len(), m.len());
        }
        for (k, v) in &m {
            assert_eq!(t.get(k), Some(*v));
        }
        assert_eq!(t.get(b"absent"), None);
    }

    #[test]
    fn removed_keys_do_not_keep_their_bytes() {
        let mut t = PlacementTable::new();
        for round in 0..20u32 {
            for i in 0..10_000u32 {
                t.insert(format!("row:{round}:{i}").as_bytes(), 1, 1);
            }
            for i in 0..10_000u32 {
                t.remove(format!("row:{round}:{i}").as_bytes());
            }
        }
        t.insert(b"last", 0, 0);
        assert!(t.arena.len() < 200_000, "the arena was rewritten: {} bytes", t.arena.len());
        assert_eq!(t.get(b"last"), Some((0, 0)));
    }

    #[test]
    fn a_row_costs_its_slot_share_and_its_key() {
        let mut t = PlacementTable::new();
        for i in 0..100_000u32 {
            t.insert(format!("user:{i}").as_bytes(), 0, 0);
        }
        let per_row = t.approx_bytes() as f64 / 100_000.0;
        assert_eq!(size_of::<Slot>(), 24);
        // 24-byte slots at 3/8 to 7/8 load, plus a ten-byte key
        assert!(per_row < 24.0 / 0.375 + 20.0, "{per_row:.1} bytes a row");
    }
}
