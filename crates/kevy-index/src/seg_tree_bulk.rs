//! Whole-tree work on a [`Tree`]: stepping through it, building it from
//! sorted entries, repacking it, cutting its low end off.

use super::{FANOUT, Path, Pos, Tree};
use crate::seg_leaf::{Ent, NIL, Probe};

/// A leaf being filled in order: its id, first key, and entry count.
type Filled = Vec<(u32, Vec<u8>, usize)>;

impl Tree {
    /// The first entry, if any.
    pub(crate) fn first_pos(&self) -> Option<Pos> {
        self.normalize(Pos { leaf: self.first, slot: 0 })
    }

    /// The last entry, if any.
    pub(crate) fn last_pos(&self) -> Option<Pos> {
        let mut node = self.root;
        for _ in 0..self.height {
            node = *self.inners[node as usize].kids.last().expect("an inner node has children");
        }
        let n = self.leaf(node).len();
        (n > 0).then(|| Pos { leaf: node, slot: n - 1 })
    }

    pub(crate) fn next_pos(&self, pos: Pos) -> Option<Pos> {
        self.normalize(Pos { leaf: pos.leaf, slot: pos.slot + 1 })
    }

    pub(crate) fn prev_pos(&self, pos: Pos) -> Option<Pos> {
        if pos.slot > 0 {
            return Some(Pos { leaf: pos.leaf, slot: pos.slot - 1 });
        }
        let prev = self.leaf(pos.leaf).prev;
        (prev != NIL).then(|| Pos { leaf: prev, slot: self.leaf(prev).len() - 1 })
    }

    /// The last entry below the probe.
    pub(crate) fn before(&self, p: &Probe<'_>) -> Option<Pos> {
        match self.lower_bound(p) {
            Some(pos) => self.prev_pos(pos),
            None => self.last_pos(),
        }
    }

    /// Entry `pos`'s order key into `key` (cleared first) and its payload.
    pub(crate) fn entry(&self, pos: Pos, key: &mut Vec<u8>) -> &[u8] {
        key.clear();
        let l = self.leaf(pos.leaf);
        l.key_into(pos.slot, &self.ov, key);
        l.tail(pos.slot, &self.ov).payload
    }

    /// Append an entry past every other, filling the current last leaf
    /// `cur` and opening a new one when it is full.
    fn append(&mut self, cur: &mut u32, filled: &mut Filled, e: Ent<'_>) {
        let (l, ov) = self.leaf_ov(*cur);
        if !l.insert_at(l.len(), e, ov) {
            let next = self.new_leaf();
            self.link_after(*cur, next);
            *cur = next;
            let (l, ov) = self.leaf_ov(next);
            let fit = l.insert_at(0, e, ov);
            debug_assert!(fit, "an empty leaf holds any one entry");
        }
        if self.leaf(*cur).len() == 1 {
            filled.push((*cur, e.key.to_vec(), 0));
        }
        self.len += 1;
        filled.last_mut().expect("the current leaf is listed").2 += 1;
    }

    /// Replace the whole tree with `entries`, which must come in order.
    pub(crate) fn rebuild<'e>(&mut self, entries: impl Iterator<Item = Ent<'e>>) {
        *self = Tree::new(self.shape);
        let (mut cur, mut filled) = (self.root, Filled::new());
        for e in entries {
            self.append(&mut cur, &mut filled, e);
        }
        self.build_inners(filled);
    }

    /// Build the inner levels over leaves given with their first keys and
    /// entry counts, in order.
    fn build_inners(&mut self, mut level: Filled) {
        while level.len() > 1 {
            let mut up = Vec::with_capacity(level.len() / FANOUT + 1);
            // an even spread, so no node starts out nearly empty
            let groups = level.len().div_ceil(FANOUT);
            let per = level.len().div_ceil(groups);
            for group in level.chunks(per) {
                let id = self.new_inner();
                let node = &mut self.inners[id as usize];
                for (j, (kid, first, count)) in group.iter().enumerate() {
                    if j > 0 {
                        self.sep_bytes += first.len();
                        node.insert_sep(j - 1, first.clone().into_boxed_slice());
                    }
                    node.kids.push(*kid);
                    node.counts.push(*count as u32);
                }
                let total = group.iter().map(|g| g.2).sum();
                up.push((id, group[0].1.clone(), total));
            }
            level = up;
            self.height += 1;
        }
        if let Some((root, _, _)) = level.pop() {
            self.root = root;
        }
    }

    /// Repack every entry into full leaves, in order. Each old leaf is
    /// freed as soon as it has been read, so the extra memory is a leaf.
    pub(crate) fn repack(&mut self) {
        let mut old = std::mem::replace(self, Tree::new(self.shape));
        let (mut cur, mut filled) = (self.root, Filled::new());
        let mut key = Vec::new();
        let mut id = old.first;
        while id != NIL {
            let l = old.leaves[id as usize].take().expect("a live leaf");
            for i in 0..l.len() {
                key.clear();
                l.key_into(i, &old.ov, &mut key);
                let t = l.tail(i, &old.ov);
                self.append(
                    &mut cur,
                    &mut filled,
                    Ent { key: &key, vlen: t.vlen, payload: t.payload },
                );
            }
            l.release_slabs(0, l.len(), &mut old.ov);
            id = l.next;
        }
        self.build_inners(filled);
    }

    /// Detach every entry below the probe, showing each to `seen` (order
    /// key, payload) in order before it goes.
    pub(crate) fn cut_below(&mut self, p: &Probe<'_>, mut seen: impl FnMut(Ent<'_>)) {
        let mut key = Vec::new();
        loop {
            let id = self.first;
            let l = self.leaf(id);
            let n = l.len();
            let cut = l.lower_bound(p, &self.ov);
            for i in 0..cut {
                key.clear();
                l.key_into(i, &self.ov, &mut key);
                let t = l.tail(i, &self.ov);
                seen(Ent { key: &key, vlen: t.vlen, payload: t.payload });
            }
            if cut == 0 {
                return;
            }
            self.drop_front(id, cut);
            if cut < n || self.len == 0 {
                return;
            }
        }
    }

    /// Remove the first `k` entries of the first leaf `id`, fixing counts
    /// along the leftmost path.
    fn drop_front(&mut self, id: u32, k: usize) {
        let mut path = Path::new();
        let mut node = self.root;
        for _ in 0..self.height {
            path.push((node, 0));
            node = self.inners[node as usize].kids[0];
        }
        debug_assert_eq!(node, id, "the first leaf is the leftmost");
        let (l, ov) = self.leaf_ov(id);
        l.remove_head(k, ov);
        self.len -= k;
        self.bump(&path, -(k as i32));
        self.settle(&mut path, id);
    }
}
