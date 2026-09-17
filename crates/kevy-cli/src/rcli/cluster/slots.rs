//! Sets of hash slots, and the `[0-99],[201]` form they are shown in.

/// The number of hash slots in a cluster.
pub(crate) const SLOTS: usize = 16384;

/// A set of hash slots, one bit each.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct SlotSet(Box<[u64; SLOTS / 64]>);

impl Default for SlotSet {
    fn default() -> Self {
        SlotSet(Box::new([0; SLOTS / 64]))
    }
}

impl std::fmt::Debug for SlotSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&String::from_utf8_lossy(&bracketed(&self.ranges())))
    }
}

impl SlotSet {
    pub(crate) fn insert(&mut self, slot: u16) {
        self.0[usize::from(slot) / 64] |= 1 << (slot % 64);
    }

    pub(crate) fn contains(&self, slot: u16) -> bool {
        self.0[usize::from(slot) / 64] & (1 << (slot % 64)) != 0
    }

    pub(crate) fn count(&self) -> usize {
        self.0.iter().map(|w| w.count_ones() as usize).sum()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.0.iter().all(|&w| w == 0)
    }

    /// The slots in ascending order.
    pub(crate) fn iter(&self) -> impl Iterator<Item = u16> + '_ {
        (0..SLOTS as u16).filter(|&s| self.contains(s))
    }

    /// Runs of consecutive slots, as inclusive `(first, last)` pairs.
    pub(crate) fn ranges(&self) -> Vec<(u16, u16)> {
        let mut out: Vec<(u16, u16)> = Vec::new();
        for slot in self.iter() {
            match out.last_mut() {
                Some((_, last)) if *last + 1 == slot => *last = slot,
                _ => out.push((slot, slot)),
            }
        }
        out
    }
}

/// `[0-99],[201-299],[300]`.
pub(crate) fn bracketed(ranges: &[(u16, u16)]) -> Vec<u8> {
    let parts: Vec<String> = ranges
        .iter()
        .map(|&(a, b)| if a == b { format!("[{a}]") } else { format!("[{a}-{b}]") })
        .collect();
    parts.join(",").into_bytes()
}

#[cfg(test)]
mod tests {
    use super::{SlotSet, bracketed};

    #[test]
    fn a_set_counts_and_shows_its_runs() {
        let mut s = SlotSet::default();
        assert!(s.is_empty());
        for slot in [0, 2, 5, 16383] {
            s.insert(slot);
        }
        assert_eq!((s.count(), s.contains(2), s.contains(1)), (4, true, false));
        assert_eq!(s.ranges(), vec![(0, 0), (2, 2), (5, 5), (16383, 16383)]);
        s.insert(1);
        assert_eq!(bracketed(&s.ranges()), b"[0-2],[5],[16383]");
    }
}
