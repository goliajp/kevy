//! Packing a [`Tree`]'s leaves in the background, a leaf at a time.
//!
//! Writes split a full leaf in two and merge only nearly empty ones, so
//! what an index holds depends on the order its writes came in. A hand
//! walks the leaves in order and pours the head of the next leaf into
//! the one it stands on for as long as the entries fit, dropping the next
//! leaf once it empties; separators and counts move with the entries. A
//! lap that pours nothing leaves every leaf but the last too full to take
//! its successor's first entry. The tree then rests until it has an
//! eighth more leaves or an eighth fewer entries than it rested with:
//! splits have opened half-empty leaves, or deletes have hollowed them.

use super::{Inner, Path, Tree};
use crate::seg_leaf::{Probe, head16_of};

/// Where the hand is and what the current lap has done.
#[derive(Debug, Default)]
pub(crate) struct Tidy {
    /// The first key of the leaf the hand stands on; a lap starts at the
    /// first leaf when `going` is false.
    hand: Vec<u8>,
    going: bool,
    moved: usize,
    /// Leaves and entries when a lap last moved nothing.
    rest: Option<(usize, usize)>,
}

impl Tree {
    /// Walk up to `leaves` steps of the hand; whether work remains.
    pub(crate) fn tidy(&mut self, t: &mut Tidy, leaves: usize) -> bool {
        if let Some((l0, n0)) = t.rest {
            let drifted = self.live_leaves() * 8 > l0 * 9 + 8 || self.len * 9 < n0 * 8;
            if !drifted {
                return false;
            }
            t.rest = None;
        }
        for _ in 0..leaves {
            let (moved, lap_done) = self.tidy_step(t);
            t.moved += moved;
            if lap_done {
                if t.moved == 0 {
                    t.rest = Some((self.live_leaves(), self.len));
                    return false;
                }
                t.moved = 0;
            }
        }
        true
    }

    /// One step: fill the hand's leaf from the head of the next one, then
    /// move on to the next unless it emptied. Entries moved, lap over.
    fn tidy_step(&mut self, t: &mut Tidy) -> (usize, bool) {
        let mut path = Path::new();
        let id = if t.going {
            self.descend(&Probe::new(&t.hand), &mut path)
        } else {
            self.leftmost(&mut path)
        };
        let Some((np, next)) = (self.height > 0).then(|| self.beside(&path, true)).flatten() else {
            t.going = false;
            return (0, true);
        };
        t.going = true;
        let k = self.leaf(id).room_for_head_of(self.leaf(next));
        if k > 0 {
            let (into, from, _) = self.two_leaves(id, next);
            from.pour_head_into(k, into);
            self.bump(&path, k as i32);
            self.bump(&np, -(k as i32));
            if self.leaf(next).is_empty() {
                // the hand stays: this leaf may take from the one after
                self.drop_emptied(&path, &np, next);
                self.hand_to(t, id);
                return (k, false);
            }
            self.boundary(&path, &np, next);
        }
        self.hand_to(t, next);
        (k, false)
    }

    fn hand_to(&self, t: &mut Tidy, id: u32) {
        t.hand.clear();
        self.leaf(id).key_into(0, &self.ov, &mut t.hand);
    }

    /// Remove leaf `id`, emptied into its left neighbour: every separator
    /// beside it becomes the following leaf's first key, so whichever one
    /// the removal keeps still separates.
    fn drop_emptied(&mut self, left: &Path, path: &Path, id: u32) {
        if let Some((yp, y)) = self.beside(path, true) {
            self.boundary(left, path, y);
            self.boundary(path, &yp, y);
        }
        self.unlink(id);
        self.free_leaf(id);
        let mut p = *path;
        let (parent, at) = p.pop().expect("a leaf with a left neighbour has a parent");
        self.remove_child(&mut p, parent, at);
        self.shrink_root();
    }

    fn leftmost(&self, path: &mut Path) -> u32 {
        path.clear();
        let mut node = self.root;
        for _ in 0..self.height {
            path.push((node, 0));
            node = self.inners[node as usize].kids[0];
        }
        node
    }

    /// The neighbouring leaf on one side and the path to it.
    fn beside(&self, path: &Path, right: bool) -> Option<(Path, u32)> {
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
    /// `right` lead to, which sits where their paths part, to leaf `id`'s
    /// first key.
    fn boundary(&mut self, left: &Path, right: &Path, id: u32) {
        let d = (0..left.len)
            .find(|&d| left.items[d].1 != right.items[d].1)
            .expect("two leaves part at some inner node");
        let (node, at) = left.items[d];
        let l = self.leaves[id as usize].as_deref().expect("a live leaf");
        let (t, head) = (l.tail(0, &self.ov), l.head(0).to_be_bytes());
        put_sep(&mut self.inners, &mut self.sep_bytes, node, at, [&head[..t.len.min(8)], t.rest]);
    }
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

#[cfg(feature = "tidy-trace")]
impl Tree {
    pub(crate) fn tidy_probe(&self, t: &Tidy) -> crate::segment_tidy::TidyProbe {
        crate::segment_tidy::TidyProbe {
            entries: self.len,
            leaves: self.live_leaves(),
            inners: self.live_inners(),
            height: self.height,
            sep_bytes: self.sep_bytes,
            overflow_bytes: self.ov.bytes,
            free_list_cap: self.free_leaves.capacity(),
            hand_cap: t.hand.capacity(),
            lap_moved: t.moved,
            resting: t.rest.is_some(),
        }
    }
}
