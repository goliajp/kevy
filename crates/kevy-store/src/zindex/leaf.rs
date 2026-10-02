//! A leaf: up to [`LEAF_CAP`] entries kept in slot order and sorted
//! through a permutation word, so an insert or a removal moves no entry.
//! The `Arc`'s counts, the word and the scores fill the allocation's first
//! two cache lines — the first one a write reads anyway for the count —
//! so a search reads those two and a member only where two scores are
//! equal.

use core::cmp::Ordering;
use core::mem;

use crate::value::SmallBytes;

/// Entries per leaf: 16 bytes of `Arc` counts, the word and 13 scores
/// make 128. (The word could order 15.)
pub(super) const LEAF_CAP: usize = 13;
/// Fewest entries a leaf other than the root keeps.
pub(super) const LEAF_MIN: usize = LEAF_CAP / 2;

/// No entries; positions 0..13 list slots 0..13, all free.
const PERM_EMPTY: u64 = 0x00CB_A987_6543_2100;

/// Bits `lo..hi` of a word.
fn bits(lo: u32, hi: u32) -> u64 {
    let below = |b: u32| if b >= 64 { u64::MAX } else { (1u64 << b) - 1 };
    below(hi) & !below(lo)
}

#[derive(Clone, Debug)]
#[repr(C)]
pub(crate) struct Leaf {
    /// Bits 0..4: how many entries. Nibble `1 + i`: the slot of the
    /// `i`-th smallest entry; past the count, the free slots.
    perm: u64,
    scores: [u64; LEAF_CAP],
    members: [SmallBytes; LEAF_CAP],
}

impl Leaf {
    pub(super) fn new() -> Self {
        Self {
            perm: PERM_EMPTY,
            scores: [0; LEAF_CAP],
            members: [const { SmallBytes::new() }; LEAF_CAP],
        }
    }

    pub(super) fn len(&self) -> usize {
        (self.perm & 15) as usize
    }

    fn slot(&self, i: usize) -> usize {
        (self.perm >> (4 + 4 * i) & 15) as usize
    }

    /// The `i`-th smallest entry's score key.
    pub(super) fn score(&self, i: usize) -> u64 {
        self.scores[self.slot(i)]
    }

    /// The `i`-th smallest entry's member.
    pub(super) fn member(&self, i: usize) -> &SmallBytes {
        &self.members[self.slot(i)]
    }

    /// Where `(sk, m)` is, or where it would go.
    pub(super) fn search(&self, sk: u64, m: &[u8]) -> Result<usize, usize> {
        let (mut lo, mut hi) = (0, self.len());
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let s = self.slot(mid);
            match self.scores[s].cmp(&sk).then_with(|| self.members[s].as_slice().cmp(m)) {
                Ordering::Less => lo = mid + 1,
                Ordering::Greater => hi = mid,
                Ordering::Equal => return Ok(mid),
            }
        }
        Err(lo)
    }

    /// How many leading entries have a score key `pred` holds for, where
    /// it holds on a prefix.
    pub(super) fn partition(&self, pred: &impl Fn(u64) -> bool) -> usize {
        let (mut lo, mut hi) = (0, self.len());
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if pred(self.score(mid)) {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo
    }

    /// Put an entry at sorted position `i`, into the first free slot.
    pub(super) fn insert_at(&mut self, i: usize, sk: u64, m: SmallBytes) {
        let n = self.len();
        let s = self.slot(n);
        self.scores[s] = sk;
        self.members[s] = m;
        let (at, free) = (4 + 4 * i as u32, 4 + 4 * n as u32);
        let p = self.perm;
        self.perm = p & bits(4, at)
            | (p & bits(at, free)) << 4
            | (s as u64) << at
            | p & bits(free + 4, 64)
            | (n as u64 + 1);
    }

    /// Take out the entry at sorted position `i`; its slot goes free.
    pub(super) fn remove_at(&mut self, i: usize) -> (u64, SmallBytes) {
        let n = self.len();
        let s = self.slot(i);
        let (at, last) = (4 + 4 * i as u32, 4 * n as u32);
        let p = self.perm;
        self.perm = p & bits(4, at)
            | (p & bits(at + 4, last + 4)) >> 4
            | (s as u64) << last
            | p & bits(last + 4, 64)
            | (n as u64 - 1);
        (self.scores[s], mem::replace(&mut self.members[s], SmallBytes::new()))
    }

    pub(super) fn push(&mut self, sk: u64, m: SmallBytes) {
        self.insert_at(self.len(), sk, m);
    }

    /// Move the entries from sorted position `at` on into a new leaf.
    pub(super) fn split_off(&mut self, at: usize) -> Leaf {
        let mut right = Leaf::new();
        for i in at..self.len() {
            let s = self.slot(i);
            right.push(self.scores[s], mem::replace(&mut self.members[s], SmallBytes::new()));
        }
        // the moved entries' slots sit right past the new count: free
        self.perm = self.perm & !15 | at as u64;
        right
    }

    /// Move every entry of `right`, all above this leaf's, onto its end.
    pub(super) fn append(&mut self, mut right: Leaf) {
        for i in 0..right.len() {
            let s = right.slot(i);
            self.push(right.scores[s], mem::replace(&mut right.members[s], SmallBytes::new()));
        }
    }

    /// Whether the permutation lists every slot exactly once.
    #[cfg(test)]
    pub(super) fn perm_is_whole(&self) -> bool {
        let mut seen = 0u16;
        (0..LEAF_CAP).for_each(|i| seen |= 1 << self.slot(i));
        seen == (1 << LEAF_CAP) - 1
    }
}
