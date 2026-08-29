//! Incremental, rehash-tolerant table scanning — the cursor arithmetic
//! behind a Redis-style `SCAN`.
//!
//! [`KevyMap::scan_step`] visits one *home bucket-group* (16 buckets) per
//! call and returns the next cursor (0 = the sweep is complete). The
//! cursor walks group indices in **reverse-binary-increment** order —
//! Redis `dictScan`'s algorithm — masked to the CURRENT table size on
//! every call. Because the table's capacity is always a power of two and
//! an entry's home group in a doubled table is its old home group plus
//! one new high bit, this ordering guarantees: **every key present in
//! the map for the whole duration of a sweep is visited at least once,
//! even if the table grows (rehashes) between calls.** Keys may be
//! visited more than once across a grow; callers must tolerate
//! duplicates (Redis SCAN has the same contract).
//!
//! Displacement: `KevyMap` is open-addressing, so an entry's *stored*
//! slot is not a pure function of its hash — a rehash can move it
//! relative to other entries, which would break the guarantee if we
//! enumerated stored positions. We therefore enumerate by **home**
//! bucket (`hash & mask`, a pure function of hash and capacity) and,
//! for each home group, walk the probe run forward collecting entries
//! whose recomputed home falls in the group. See
//! [`KevyMap::visit_home_group`] for the run-termination proof.

use kevy_hash::KevyHash;

use crate::map::{EMPTY, GROUP_WIDTH, KevyMap};

impl<K: KevyHash + Eq, V> KevyMap<K, V> {
    /// Visit one bucket-group's worth of the map and return the next
    /// cursor. Start a sweep with `cursor = 0`; a returned `0` means the
    /// sweep is complete. Grows between calls never skip a key that was
    /// present throughout (see the module docs); shrinks don't exist
    /// (`KevyMap` never shrinks in place).
    ///
    /// `f` is called once per entry whose home group is the cursor's
    /// group — at most once per entry per call, in unspecified order.
    ///
    /// Bounded work: one call touches one 16-bucket home group plus its
    /// probe-run overhang (short at the 7/8 load factor).
    /// # Examples
    ///
    /// ```
    /// let mut m = kevy_map::KevyMap::new();
    /// for i in 0..64u32 { m.insert(i, i); }
    ///
    /// // A SCAN-style cursor: bounded work per call, 0 both starts and
    /// // ends the walk, and every key is seen at least once across it.
    /// let mut seen = Vec::new();
    /// let mut cur = 0u64;
    /// loop {
    ///     cur = m.scan_step(cur, |k, _| seen.push(*k));
    ///     if cur == 0 { break; }
    /// }
    /// seen.sort_unstable();
    /// seen.dedup();
    /// assert_eq!(seen.len(), 64);
    /// ```
    pub fn scan_step(&self, cursor: u64, mut f: impl FnMut(&K, &V)) -> u64 {
        if self.cap == 0 {
            return 0;
        }
        // cap is a power of two ≥ MIN_CAP (16), so ngroups ≥ 1 and gmask
        // is a low-bit mask.
        let ngroups = (self.cap / GROUP_WIDTH) as u64;
        let gmask = ngroups - 1;
        self.visit_home_group((cursor & gmask) as usize, &mut f);
        // dictScan's reverse-binary increment: force every bit above the
        // mask to 1 so the +1 carry (performed in the bit-reversed
        // domain) clears them — the result is always ≤ gmask, and a
        // cursor minted against a smaller table resumes correctly
        // against a larger one.
        let v = (cursor | !gmask).reverse_bits();
        v.wrapping_add(1).reverse_bits()
    }

    /// Call `f` for every entry whose *home* bucket (`hash & mask`) lies
    /// in aligned group `g` (buckets `[16g, 16g + 16)`).
    ///
    /// Walk + termination: insert probes 16-wide windows starting at the
    /// home bucket, so an entry with home `h` sits at `p ≥ h` where the
    /// contiguous range `[h, window(p))` holds no EMPTY — except that
    /// tombstone reuse may place it up to `GROUP_WIDTH - 1` slots past
    /// the first EMPTY at-or-after `h` (both inside one window). EMPTY
    /// slots are only ever consumed (deletes leave DELETED; only a grow
    /// mints a fresh all-EMPTY table), so the bound never decays.
    /// Therefore every group-`g` entry lies at or before
    /// `first-EMPTY-at-or-after(16g + 15) + GROUP_WIDTH - 1`, which is
    /// exactly where this walk stops.
    fn visit_home_group(&self, g: usize, f: &mut impl FnMut(&K, &V)) {
        let start = g * GROUP_WIDTH;
        let mut past_empty: Option<usize> = None;
        // `off` covers at most the whole table (a live table always has
        // ≥ cap/8 EMPTY slots — occupied + deleted never exceeds the 7/8
        // threshold — so the walk terminates far earlier in practice).
        for off in 0..self.cap {
            let p = (start + off) & self.mask;
            // SAFETY: p < cap ⇒ metadata pointer in-bounds.
            let meta = unsafe { *self.metadata_ptr.as_ptr().add(p) };
            if meta & 0x80 == 0 {
                // SAFETY: occupied slot ⇒ initialised.
                let kv = unsafe { (*self.slots_ptr.as_ptr().add(p)).assume_init_ref() };
                let home = (kv.0.kevy_hash() as usize) & self.mask;
                if home / GROUP_WIDTH == g {
                    f(&kv.0, &kv.1);
                }
            }
            match &mut past_empty {
                Some(0) => return,
                Some(t) => *t -= 1,
                // The terminating EMPTY must sit at-or-after the LAST
                // home bucket of the group (off ≥ GROUP_WIDTH - 1) so it
                // bounds every home in the group, not just the first.
                None if meta == EMPTY && off + 1 >= GROUP_WIDTH => {
                    past_empty = Some(GROUP_WIDTH - 1);
                }
                None => {}
            }
        }
    }
}

#[cfg(test)]
#[path = "scan_tests.rs"]
mod tests;
