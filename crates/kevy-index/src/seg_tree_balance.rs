//! Keeping every leaf but the tree's first and last at least two-thirds
//! full, whatever order entries arrive and leave in.
//!
//! A full leaf whose new entry lands on its edge hands the entry to the
//! neighbour on that side; otherwise it deals its entries, the new one
//! and a neighbour's evenly over the two leaves, and only when both are
//! full over three. A leaf that drops below two-thirds on a delete deals
//! itself and both neighbours over as few leaves as hold them. Entries
//! cross between leaves under different parents: the separator between
//! two neighbouring leaves sits in the deepest inner node both descend
//! through.
//!
//! The first and last leaf are exempt so that runs of ascending or
//! descending writes keep opening a fresh end leaf and leave every other
//! leaf full.

use super::deal::Group;
use super::{Inner, Path, Tree};
use crate::seg_leaf::{Ent, Leaf, NIL, head16_of};

/// Bytes a leaf that is neither the tree's first nor its last holds,
/// short by at most two entries: a cut between leaves falls on an entry
/// boundary, up to one entry off at each end.
pub(crate) const FILL: usize = Leaf::capacity() * 2 / 3;

impl Tree {
    /// Leaf `id` at the end of `path` has no room for `e` at slot `at`.
    pub(crate) fn overflow(&mut self, path: &mut Path, id: u32, at: usize, e: Ent<'_>) {
        let l = self.leaf(id);
        let (n, first, last) = (l.len(), l.prev == NIL, l.next == NIL);
        if path.len == 0 || (at == n && last) || (at == 0 && first) {
            // the root leaf, or a new end of the tree: open a leaf
            self.split_insert(path, id, at, e);
            return;
        }
        if (at == n || at == 0) && self.place_beside(path, id, at == n, e) {
            return;
        }
        let side = [false, true].map(|right| self.beside(path, right));
        let free = |s: &Option<(Path, u32)>| {
            s.as_ref().map(|&(_, x)| Leaf::capacity() - self.leaf(x).used())
        };
        let right = free(&side[1]) > free(&side[0]);
        let (sp, sid) = side[usize::from(right)].expect("a leaf below the root has a neighbour");
        let mut g = Group::default();
        if !right {
            g.push(self, sid, sp);
        }
        g.push(self, id, *path);
        g.incoming(g.k - 1, at, self.leaf(id).span_of(&e));
        if right {
            g.push(self, sid, sp);
        }
        let dealt = self.deal(&g, Some(e), 2) || self.deal(&g, Some(e), 3);
        debug_assert!(dealt, "two full leaves deal over three");
    }

    /// Put `e` into the neighbour past this leaf's edge when it has room.
    fn place_beside(&mut self, path: &Path, id: u32, right: bool, e: Ent<'_>) -> bool {
        let (sp, sid) = self.beside(path, right).expect("a leaf past this edge");
        let (s, ov) = self.leaf_ov(sid);
        let slot = if right { 0 } else { s.len() };
        if !s.insert_at(slot, e, ov) {
            return false;
        }
        self.bump(&sp, 1);
        if right {
            let (node, at) = parting(path, &sp);
            put_sep(&mut self.inners, &mut self.sep_bytes, node, at, [e.key, &[]]);
        } else {
            self.boundary(&sp, path, id);
        }
        true
    }

    /// Leaf `id` at the end of `path` has thinned below [`FILL`]: deal it
    /// and its neighbours over as few leaves as hold them. An end leaf
    /// only merges into its neighbour.
    pub(crate) fn refill(&mut self, path: &Path, id: u32) {
        let mut g = Group::default();
        if let Some((p, x)) = self.beside(path, false) {
            g.push(self, x, p);
        }
        g.push(self, id, *path);
        if let Some((p, x)) = self.beside(path, true) {
            g.push(self, x, p);
        }
        let most = if g.k == 3 { 3 } else { 1 };
        let _ = (1..=most).any(|m| self.deal(&g, None, m));
    }

    /// The neighbouring leaf on one side and the path to it.
    pub(crate) fn beside(&self, path: &Path, right: bool) -> Option<(Path, u32)> {
        let mut p = *path;
        let depth = (0..p.len).rev().find(|&d| {
            let (node, at) = p.items[d];
            if right { at + 1 < self.inners[node as usize].kids.len() } else { at > 0 }
        })?;
        p.len = depth + 1;
        let step = &mut p.items[depth].1;
        *step = if right { *step + 1 } else { *step - 1 };
        let (node, at) = p.items[depth];
        let mut id = self.inners[node as usize].kids[at];
        for _ in depth + 1..self.height {
            let inner = &self.inners[id as usize];
            let at = if right { 0 } else { inner.kids.len() - 1 };
            p.push((id, at));
            id = inner.kids[at];
        }
        Some((p, id))
    }

    /// Set the separator between the neighbouring leaves `left` and
    /// `right` lead to to leaf `id`'s first key.
    pub(crate) fn boundary(&mut self, left: &Path, right: &Path, id: u32) {
        let (node, at) = parting(left, right);
        let l = self.leaves[id as usize].as_deref().expect("a live leaf");
        let (t, head) = (l.tail(0, &self.ov), l.head(0).to_be_bytes());
        let parts = [&head[..t.len.min(8)], t.rest];
        put_sep(&mut self.inners, &mut self.sep_bytes, node, at, parts);
    }
}

/// The inner node and separator between two neighbouring leaves: where
/// their paths part.
fn parting(left: &Path, right: &Path) -> (u32, usize) {
    let d = (0..left.len)
        .find(|&d| left.items[d].1 != right.items[d].1)
        .expect("two leaves part at some inner node");
    left.items[d]
}

/// Overwrite separator `at` of `node` with the bytes of `parts`, in place
/// when its length is unchanged.
fn put_sep(inners: &mut [Inner], sep_bytes: &mut usize, node: u32, at: usize, parts: [&[u8]; 2]) {
    let (n, len) = (&mut inners[node as usize], parts[0].len() + parts[1].len());
    if n.seps[at].len() != len {
        *sep_bytes = *sep_bytes + len - n.seps[at].len();
        n.seps[at] = vec![0; len].into_boxed_slice();
    }
    let s = &mut n.seps[at];
    s[..parts[0].len()].copy_from_slice(parts[0]);
    s[parts[0].len()..].copy_from_slice(parts[1]);
    n.heads[at] = head16_of(s);
}
