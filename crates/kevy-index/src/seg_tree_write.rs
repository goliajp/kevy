//! Inserting into and removing from a [`Tree`].

use std::cmp::Ordering;

use super::{FANOUT, Path, Pos, Tree};
use crate::seg_leaf::{Ent, Leaf, Probe, head16_of};

impl Tree {
    /// Insert `key` with `payload`. An equal key already there has its
    /// payload replaced; returns whether the key is new.
    #[cfg(test)]
    pub(crate) fn insert(&mut self, key: &[u8], payload: &[u8]) -> bool {
        self.insert_seen(Ent { key, vlen: 0, payload }, |_, _| {})
    }

    /// [`Tree::insert`], showing `seen` where a new key goes (the slot it
    /// will take, possibly one past its leaf's end) before the tree
    /// changes.
    pub(crate) fn insert_seen(&mut self, e: Ent<'_>, seen: impl FnOnce(&Tree, Pos)) -> bool {
        self.ensure_root();
        let p = Probe::new(e.key);
        let mut path = Path::new();
        let id = self.descend(&p, &mut path);
        let at = self.leaf(id).lower_bound(&p, &self.ov);
        let found =
            at < self.leaf(id).len() && self.leaf(id).cmp_at(&p, at, &self.ov) == Ordering::Equal;
        if !found {
            seen(self, Pos { leaf: id, slot: at });
        }
        if found && self.leaf(id).tail(at, &self.ov).payload == e.payload {
            return false;
        }
        if found {
            let (l, ov) = self.leaf_ov(id);
            l.remove_at(at, ov);
            if l.insert_at(at, e, ov) {
                return false;
            }
            // a longer payload no longer fits: count it out, then back in
            self.bump(&path, -1);
            self.len -= 1;
        }
        self.len += 1;
        let (l, ov) = self.leaf_ov(id);
        if l.insert_at(at, e, ov) {
            self.bump(&path, 1);
        } else if !self.place_beside(&path, id, at, e) {
            self.split_insert(&mut path, id, at, e);
        }
        !found
    }

    /// A full leaf whose new entry lands on its edge: put the entry into
    /// the sibling on that side instead, when they share a parent and it
    /// has room. Keeps runs of ascending or descending writes packed.
    fn place_beside(&mut self, path: &Path, id: u32, at: usize, e: Ent<'_>) -> bool {
        let Some(&(parent, i)) = path.last() else { return false };
        let n = self.leaf(id).len();
        let kids = &self.inners[parent as usize].kids;
        let (sib, front) = match at {
            _ if at == n && i + 1 < kids.len() => (kids[i + 1], true),
            0 if i > 0 => (kids[i - 1], false),
            _ => return false,
        };
        let (s, ov) = self.leaf_ov(sib);
        let slot = if front { 0 } else { s.len() };
        if !s.insert_at(slot, e, ov) {
            return false;
        }
        let mut sib_path = *path;
        sib_path.last_mut().expect("a parent").1 = if front { i + 1 } else { i - 1 };
        self.bump(&sib_path, 1);
        // the separator between the two leaves drops to the new entry
        // (front) or rises to this leaf's first entry (back)
        let mut sep = Vec::new();
        let sep_at = if front {
            sep.extend_from_slice(e.key);
            i
        } else {
            self.leaf(id).key_into(0, &self.ov, &mut sep);
            i - 1
        };
        self.set_sep(parent, sep_at, sep);
        true
    }

    fn set_sep(&mut self, node: u32, at: usize, sep: Vec<u8>) {
        let n = &mut self.inners[node as usize];
        self.sep_bytes = self.sep_bytes + sep.len() - n.seps[at].len();
        n.heads[at] = head16_of(&sep);
        n.seps[at] = sep.into_boxed_slice();
    }

    /// Split full leaf `id` to make room for `key` at `at`: at the insert
    /// point when it is an edge (the new entry opens a leaf of its own),
    /// otherwise down the middle by bytes.
    fn split_insert(&mut self, path: &mut Path, id: u32, at: usize, e: Ent<'_>) {
        let n = self.leaf(id).len();
        let new = self.new_leaf();
        if at == 0 {
            let (l, ov) = self.leaf_ov(new);
            l.insert_at(0, e, ov);
            let mut sep = Vec::new();
            self.leaf(id).key_into(0, &self.ov, &mut sep);
            self.link_before(id, new);
            self.hang_before(path, id, new, sep);
            return;
        }
        let cut = if at == n { n } else { self.middle(id) };
        let (left, right, ov) = self.two_leaves(id, new);
        left.move_tail_to(cut, right);
        let fit = if at < cut || (at == cut && at < n) {
            left.insert_at(at, e, ov)
        } else {
            right.insert_at(at - cut, e, ov)
        };
        debug_assert!(fit, "half a leaf has room for one entry");
        let mut sep = Vec::new();
        self.leaf(new).key_into(0, &self.ov, &mut sep);
        self.link_after(id, new);
        let moved = self.leaf(new).len();
        if let Some(&(parent, i)) = path.last() {
            let c = &mut self.inners[parent as usize].counts[i];
            *c = *c + 1 - moved as u32;
        }
        self.bump_above(path, 1);
        self.add_child(path, new, moved, sep.into_boxed_slice());
    }

    /// `new` (holding one entry) becomes the child just before `id`.
    fn hang_before(&mut self, path: &mut Path, id: u32, new: u32, sep: Vec<u8>) {
        self.bump_above(path, 1);
        let Some((parent, i)) = path.pop() else {
            // `id` was the root leaf: grow a root over `new` then `id`
            self.root = new;
            self.add_child(path, id, self.len - 1, sep.into_boxed_slice());
            return;
        };
        self.sep_bytes += sep.len();
        let p = &mut self.inners[parent as usize];
        p.insert_sep(i, sep.into_boxed_slice());
        p.kids.insert(i, new);
        p.counts.insert(i, 1);
        if p.kids.len() > FANOUT {
            self.split_inner(path, parent);
        }
    }

    /// Add `delta` to every count on `path` but the last, which the caller
    /// fixes up itself.
    fn bump_above(&mut self, path: &Path, delta: i32) {
        for &(node, at) in path.iter().rev().skip(1) {
            let c = &mut self.inners[node as usize].counts[at];
            *c = c.wrapping_add_signed(delta);
        }
    }

    /// The slot that splits leaf `id` into two halves by bytes.
    fn middle(&self, id: u32) -> usize {
        let l = self.leaf(id);
        let half = l.used() / 2;
        let mut acc = 0;
        for i in 0..l.len() {
            acc += l.span_bytes(i, i + 1);
            if acc >= half {
                return (i + 1).clamp(1, l.len() - 1);
            }
        }
        l.len() / 2
    }

    pub(crate) fn two_leaves(
        &mut self,
        a: u32,
        b: u32,
    ) -> (&mut Leaf, &mut Leaf, &mut crate::seg_leaf::Overflow) {
        let (a, b) = (a as usize, b as usize);
        let (x, y) = if a < b {
            let (lo, hi) = self.leaves.split_at_mut(b);
            (&mut lo[a], &mut hi[0])
        } else {
            let (lo, hi) = self.leaves.split_at_mut(a);
            (&mut hi[0], &mut lo[b])
        };
        let live = "a live leaf";
        (x.as_deref_mut().expect(live), y.as_deref_mut().expect(live), &mut self.ov)
    }

    /// Remove `key`; returns whether it was there.
    #[cfg(test)]
    pub(crate) fn remove(&mut self, key: &[u8]) -> bool {
        self.remove_seen(key, |_, _| {})
    }

    /// [`Tree::remove`], showing `seen` the entry before it goes.
    pub(crate) fn remove_seen(&mut self, key: &[u8], seen: impl FnOnce(&Tree, Pos)) -> bool {
        if self.root == crate::seg_leaf::NIL {
            return false;
        }
        let p = Probe::new(key);
        let mut path = Path::new();
        let id = self.descend(&p, &mut path);
        let at = self.leaf(id).lower_bound(&p, &self.ov);
        if at == self.leaf(id).len() || self.leaf(id).cmp_at(&p, at, &self.ov) != Ordering::Equal {
            return false;
        }
        seen(self, Pos { leaf: id, slot: at });
        let (l, ov) = self.leaf_ov(id);
        l.remove_at(at, ov);
        self.len -= 1;
        self.bump(&path, -1);
        self.settle(&mut path, id);
        true
    }

    /// After a removal from leaf `id`: drop it when empty, merge it with a
    /// sibling when it has thinned out and the two fit in one.
    pub(crate) fn settle(&mut self, path: &mut Path, id: u32) {
        let Some(&(parent, i)) = path.last() else {
            // the root leaf: an empty tree holds none
            if self.leaf(id).is_empty() {
                self.free_leaf(id);
                (self.root, self.first) = (crate::seg_leaf::NIL, crate::seg_leaf::NIL);
            }
            return;
        };
        let l = self.leaf(id);
        if l.is_empty() {
            path.pop();
            self.unlink(id);
            self.free_leaf(id);
            self.remove_child(path, parent, i);
            self.shrink_root();
            return;
        }
        if l.used() * 4 >= Leaf::capacity() {
            return;
        }
        let kids = &self.inners[parent as usize].kids;
        let right = (i + 1 < kids.len()).then(|| (kids[i], kids[i + 1], i + 1));
        let left = (i > 0).then(|| (kids[i - 1], kids[i], i));
        for (a, b, gone) in [right, left].into_iter().flatten() {
            if self.leaf(a).used() + self.leaf(b).used() <= Leaf::capacity() * 3 / 4 {
                self.merge(path, a, b, gone);
                return;
            }
        }
    }

    /// Move leaf `b`'s entries onto the end of `a`, its left sibling under
    /// the same parent (child `gone - 1`), and drop `b`.
    fn merge(&mut self, path: &mut Path, a: u32, b: u32, gone: usize) {
        let (left, right, _) = self.two_leaves(a, b);
        right.move_tail_to(0, left);
        let (parent, _) = path.pop().expect("a parent");
        let p = &mut self.inners[parent as usize];
        p.counts[gone - 1] += p.counts[gone];
        p.counts[gone] = 0;
        self.unlink(b);
        self.free_leaf(b);
        self.remove_child(path, parent, gone);
        self.shrink_root();
    }
}
