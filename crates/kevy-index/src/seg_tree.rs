//! The ordered set under a [`crate::Segment`]: a B+ tree whose leaves
//! are packed pages ([`Leaf`]) and whose inner nodes carry, per child, a
//! separator and the number of entries below it.
//!
//! Nodes live in two arenas and point at each other by index. An inner
//! node's separator `j` sits between children `j` and `j + 1`: every
//! entry under child `j` is below it and every entry under child `j + 1`
//! is at or above it. Separators are never tightened after a delete — a
//! stale one still separates — so the only writes to them are splits,
//! merges and the neighbour placement in [`Tree::insert`].

use std::cmp::Ordering;

use crate::seg_leaf::{Leaf, NIL, Overflow, Probe, cmp_key};

/// Children an inner node holds before it splits.
pub(crate) const FANOUT: usize = 64;

#[derive(Debug, Default)]
pub(crate) struct Inner {
    pub(crate) heads: Vec<u64>,
    pub(crate) seps: Vec<Box<[u8]>>,
    pub(crate) kids: Vec<u32>,
    pub(crate) counts: Vec<u32>,
}

impl Inner {
    fn with_capacity() -> Inner {
        Inner {
            heads: Vec::with_capacity(FANOUT + 1),
            seps: Vec::with_capacity(FANOUT + 1),
            kids: Vec::with_capacity(FANOUT + 1),
            counts: Vec::with_capacity(FANOUT + 1),
        }
    }

    /// Which child a probe descends into: the number of separators at or
    /// below it.
    pub(crate) fn route(&self, p: &Probe<'_>) -> usize {
        let (mut lo, mut hi) = (0, self.seps.len());
        while lo < hi {
            let mid = (lo + hi) / 2;
            let s = &self.seps[mid];
            if cmp_key(p, self.heads[mid], s.len(), s.get(8..).unwrap_or(&[])) == Ordering::Less {
                hi = mid;
            } else {
                lo = mid + 1;
            }
        }
        lo
    }

    fn insert_sep(&mut self, at: usize, sep: Box<[u8]>) {
        self.heads.insert(at, crate::seg_leaf::head_of(&sep));
        self.seps.insert(at, sep);
    }

    fn remove_sep(&mut self, at: usize) -> Box<[u8]> {
        self.heads.remove(at);
        self.seps.remove(at)
    }
}

/// Tree height a [`Path`] can record. A root splits only at `FANOUT`
/// children and splits leave both halves half full, so a tree this tall
/// would hold more than 2^60 leaves.
const MAX_HEIGHT: usize = 16;

/// The inner nodes a descent went through and the child taken in each,
/// kept on the stack.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Path {
    items: [(u32, usize); MAX_HEIGHT],
    len: usize,
}

impl Path {
    pub(crate) fn new() -> Path {
        Path { items: [(0, 0); MAX_HEIGHT], len: 0 }
    }

    pub(crate) fn clear(&mut self) {
        self.len = 0;
    }

    pub(crate) fn push(&mut self, step: (u32, usize)) {
        self.items[self.len] = step;
        self.len += 1;
    }

    pub(crate) fn pop(&mut self) -> Option<(u32, usize)> {
        self.len = self.len.checked_sub(1)?;
        Some(self.items[self.len])
    }

    pub(crate) fn last(&self) -> Option<&(u32, usize)> {
        self.len.checked_sub(1).map(|i| &self.items[i])
    }

    pub(crate) fn last_mut(&mut self) -> Option<&mut (u32, usize)> {
        self.len.checked_sub(1).map(|i| &mut self.items[i])
    }

    pub(crate) fn iter(&self) -> std::slice::Iter<'_, (u32, usize)> {
        self.items[..self.len].iter()
    }
}

/// A slot in a leaf.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Pos {
    pub(crate) leaf: u32,
    pub(crate) slot: usize,
}

#[derive(Debug)]
pub(crate) struct Tree {
    pub(crate) leaves: Vec<Option<Box<Leaf>>>,
    free_leaves: Vec<u32>,
    pub(crate) inners: Vec<Inner>,
    free_inners: Vec<u32>,
    pub(crate) root: u32,
    pub(crate) height: usize,
    pub(crate) len: usize,
    pub(crate) payloads: bool,
    pub(crate) first: u32,
    /// Bytes held by separators, kept as they change.
    pub(crate) sep_bytes: usize,
    /// Tails of entries too big for a page.
    pub(crate) ov: Overflow,
}

impl Tree {
    pub(crate) fn new(payloads: bool) -> Tree {
        Tree {
            leaves: vec![Some(Leaf::new(payloads))],
            free_leaves: Vec::new(),
            inners: Vec::new(),
            free_inners: Vec::new(),
            root: 0,
            height: 0,
            len: 0,
            payloads,
            first: 0,
            sep_bytes: 0,
            ov: Overflow::default(),
        }
    }

    pub(crate) fn leaf(&self, id: u32) -> &Leaf {
        self.leaves[id as usize].as_deref().expect("a live leaf")
    }

    pub(crate) fn leaf_mut(&mut self, id: u32) -> &mut Leaf {
        self.leaves[id as usize].as_deref_mut().expect("a live leaf")
    }

    /// A leaf and the overflow store together, for writes that may add or
    /// release out-of-line tails.
    pub(crate) fn leaf_ov(&mut self, id: u32) -> (&mut Leaf, &mut Overflow) {
        (self.leaves[id as usize].as_deref_mut().expect("a live leaf"), &mut self.ov)
    }

    pub(crate) fn new_leaf(&mut self) -> u32 {
        let leaf = Leaf::new(self.payloads);
        match self.free_leaves.pop() {
            Some(id) => {
                self.leaves[id as usize] = Some(leaf);
                id
            }
            None => {
                self.leaves.push(Some(leaf));
                (self.leaves.len() - 1) as u32
            }
        }
    }

    pub(crate) fn free_leaf(&mut self, id: u32) {
        self.leaves[id as usize] = None;
        self.free_leaves.push(id);
    }

    pub(crate) fn new_inner(&mut self) -> u32 {
        match self.free_inners.pop() {
            Some(id) => {
                self.inners[id as usize] = Inner::with_capacity();
                id
            }
            None => {
                self.inners.push(Inner::with_capacity());
                (self.inners.len() - 1) as u32
            }
        }
    }

    fn free_inner(&mut self, id: u32) {
        let node = std::mem::take(&mut self.inners[id as usize]);
        self.sep_bytes -= node.seps.iter().map(|s| s.len()).sum::<usize>();
        self.free_inners.push(id);
    }

    /// Bytes the tree holds on the heap: its leaves, its inner nodes'
    /// arrays and separators, out-of-line entries, and the two arenas.
    pub(crate) fn heap_bytes(&self) -> usize {
        let inner_arrays = (FANOUT + 1) * (8 + std::mem::size_of::<Box<[u8]>>() + 4 + 4);
        self.live_leaves() * crate::seg_leaf::LEAF_BYTES
            + self.live_inners() * inner_arrays
            + self.sep_bytes
            + self.ov.bytes
            + self.leaves.capacity() * std::mem::size_of::<Option<Box<Leaf>>>()
            + self.inners.capacity() * std::mem::size_of::<Inner>()
    }

    pub(crate) fn live_leaves(&self) -> usize {
        self.leaves.len() - self.free_leaves.len()
    }

    pub(crate) fn live_inners(&self) -> usize {
        self.inners.len() - self.free_inners.len()
    }

    /// The leaf a probe belongs in, and the path to it.
    pub(crate) fn descend(&self, p: &Probe<'_>, path: &mut Path) -> u32 {
        path.clear();
        let mut node = self.root;
        for _ in 0..self.height {
            let inner = &self.inners[node as usize];
            let at = inner.route(p);
            path.push((node, at));
            node = inner.kids[at];
        }
        node
    }

    /// The first entry at or above the probe (`None` past the end).
    pub(crate) fn lower_bound(&self, p: &Probe<'_>) -> Option<Pos> {
        let mut path = Path::new();
        let leaf = self.descend(p, &mut path);
        let slot = self.leaf(leaf).lower_bound(p, &self.ov);
        self.normalize(Pos { leaf, slot })
    }

    /// A position one past a leaf's end moves to the next leaf's start.
    pub(crate) fn normalize(&self, pos: Pos) -> Option<Pos> {
        let l = self.leaf(pos.leaf);
        if pos.slot < l.len() {
            return Some(pos);
        }
        (l.next != NIL).then_some(Pos { leaf: l.next, slot: 0 })
    }

    /// Entries strictly below the probe.
    pub(crate) fn rank(&self, p: &Probe<'_>) -> usize {
        let mut node = self.root;
        let mut below = 0usize;
        for _ in 0..self.height {
            let inner = &self.inners[node as usize];
            let at = inner.route(p);
            below += inner.counts[..at].iter().map(|&c| c as usize).sum::<usize>();
            node = inner.kids[at];
        }
        below + self.leaf(node).lower_bound(p, &self.ov)
    }

    /// Add `delta` to the count of every child on `path`.
    pub(crate) fn bump(&mut self, path: &Path, delta: i32) {
        for &(node, at) in path.iter() {
            let c = &mut self.inners[node as usize].counts[at];
            *c = c.wrapping_add_signed(delta);
        }
    }

    pub(crate) fn link_after(&mut self, left: u32, new: u32) {
        let next = self.leaf(left).next;
        self.leaf_mut(new).prev = left;
        self.leaf_mut(new).next = next;
        self.leaf_mut(left).next = new;
        if next != NIL {
            self.leaf_mut(next).prev = new;
        }
    }

    pub(crate) fn link_before(&mut self, right: u32, new: u32) {
        let prev = self.leaf(right).prev;
        self.leaf_mut(new).next = right;
        self.leaf_mut(new).prev = prev;
        self.leaf_mut(right).prev = new;
        if prev == NIL {
            self.first = new;
        } else {
            self.leaf_mut(prev).next = new;
        }
    }

    pub(crate) fn unlink(&mut self, id: u32) {
        let (prev, next) = (self.leaf(id).prev, self.leaf(id).next);
        if prev == NIL {
            self.first = next;
        } else {
            self.leaf_mut(prev).next = next;
        }
        if next != NIL {
            self.leaf_mut(next).prev = prev;
        }
    }

    /// Hang `node` (holding `count` entries) off the parent at the end of
    /// `path` as the child right after `at`, separated by `sep`; splits
    /// inner nodes upward as they fill.
    pub(crate) fn add_child(&mut self, path: &mut Path, node: u32, count: usize, sep: Box<[u8]>) {
        let Some((parent, at)) = path.pop() else {
            self.grow_root(node, count, sep);
            return;
        };
        self.sep_bytes += sep.len();
        let p = &mut self.inners[parent as usize];
        p.insert_sep(at, sep);
        p.kids.insert(at + 1, node);
        p.counts.insert(at + 1, count as u32);
        if p.kids.len() > FANOUT {
            self.split_inner(path, parent);
        }
    }

    /// A new root above the old one and `node`.
    fn grow_root(&mut self, node: u32, count: usize, sep: Box<[u8]>) {
        let total = self.len;
        let old = self.root;
        let id = self.new_inner();
        self.sep_bytes += sep.len();
        let r = &mut self.inners[id as usize];
        r.insert_sep(0, sep);
        r.kids.extend([old, node]);
        r.counts.extend([(total - count) as u32, count as u32]);
        self.root = id;
        self.height += 1;
    }

    fn split_inner(&mut self, path: &mut Path, id: u32) {
        let right = self.new_inner();
        let (sep, moved) = {
            let left = &mut self.inners[id as usize];
            let mid = left.kids.len() / 2;
            let heads = left.heads.split_off(mid);
            let seps = left.seps.split_off(mid);
            let kids = left.kids.split_off(mid);
            let counts = left.counts.split_off(mid);
            left.heads.pop();
            let sep = left.seps.pop().expect("a separator before the split point");
            (sep, (heads, seps, kids, counts))
        };
        let r = &mut self.inners[right as usize];
        r.heads.extend(moved.0);
        r.seps.extend(moved.1);
        r.kids.extend(moved.2);
        r.counts.extend(moved.3);
        let count: usize = r.counts.iter().map(|&c| c as usize).sum();
        self.sep_bytes -= sep.len();
        if let Some(&(parent, at)) = path.last() {
            self.inners[parent as usize].counts[at] -= count as u32;
        }
        self.add_child(path, right, count, sep);
    }

    /// Take child `at` out of inner `node`, dropping the separator next to
    /// it; an inner node left with no children goes too.
    pub(crate) fn remove_child(&mut self, path: &mut Path, node: u32, at: usize) {
        let inner = &mut self.inners[node as usize];
        inner.kids.remove(at);
        inner.counts.remove(at);
        if !inner.seps.is_empty() {
            // the separator on the left: a merge moved this child's entries left
            let s = inner.remove_sep(at.saturating_sub(1));
            self.sep_bytes -= s.len();
        }
        if !self.inners[node as usize].kids.is_empty() {
            return;
        }
        self.free_inner(node);
        match path.pop() {
            Some((parent, i)) => self.remove_child(path, parent, i),
            None => {
                // the whole tree emptied through this node
                let leaf = self.new_leaf();
                (self.root, self.height, self.first) = (leaf, 0, leaf);
            }
        }
    }

    /// Drop root levels that have a single child.
    pub(crate) fn shrink_root(&mut self) {
        while self.height > 0 && self.inners[self.root as usize].kids.len() == 1 {
            let old = self.root;
            self.root = self.inners[old as usize].kids[0];
            self.height -= 1;
            self.free_inner(old);
        }
    }
}

#[path = "seg_tree_write.rs"]
mod write;

#[path = "seg_tree_bulk.rs"]
pub(crate) mod bulk;

#[cfg(test)]
#[path = "seg_tree_tests.rs"]
pub(crate) mod tests;
