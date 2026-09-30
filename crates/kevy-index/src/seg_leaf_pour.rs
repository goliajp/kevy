//! Pouring the head of one leaf into the end of the one before it, for
//! the background repack.

use super::{Leaf, SLOT};

impl Leaf {
    /// How many of `from`'s first entries fit after this leaf's last.
    pub(crate) fn room_for_head_of(&self, from: &Leaf) -> usize {
        let mut room = Self::capacity() - self.used();
        for i in 0..from.len() {
            let span = SLOT + from.tail_len(i).0;
            if span > room {
                return i;
            }
            room -= span;
        }
        from.len()
    }

    /// Move this leaf's first `k` entries to the end of `into`, which has
    /// room for them, tails as they are.
    pub(crate) fn pour_head_into(&mut self, k: usize, into: &mut Leaf) {
        for j in 0..k {
            into.copy_entry(into.len(), self, j);
        }
        let bytes = (0..k).map(|i| self.tail_len(i).0).sum();
        self.drop_slots(0, k, bytes);
    }
}
