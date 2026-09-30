//! Dealing the entries of up to three neighbouring leaves, and one on
//! its way in, over one to three leaves by bytes.

use super::balance::FILL;
use super::{Path, Tree};
use crate::seg_leaf::{Ent, Leaf, NIL};

/// Up to three neighbouring leaves, in order, about to be dealt.
#[derive(Default)]
pub(crate) struct Group {
    ids: [u32; 3],
    paths: [Path; 3],
    ns: [usize; 3],
    pub(crate) k: usize,
    /// An entry that did not fit: its leaf in the group, slot and bytes.
    into: Option<(usize, usize, usize)>,
}

impl Group {
    pub(crate) fn push(&mut self, t: &Tree, id: u32, path: Path) {
        (self.ids[self.k], self.paths[self.k]) = (id, path);
        self.ns[self.k] = t.leaf(id).len();
        self.k += 1;
    }

    pub(crate) fn incoming(&mut self, leaf: usize, slot: usize, bytes: usize) {
        self.into = Some((leaf, slot, bytes));
    }

    /// Which of `m` parts dealt from the group are the tree's first or
    /// last leaf.
    fn ends(&self, t: &Tree, m: usize) -> [bool; 3] {
        let mut ex = [false; 3];
        ex[0] = t.leaf(self.ids[0]).prev == NIL;
        ex[m - 1] |= t.leaf(self.ids[self.k - 1]).next == NIL;
        ex
    }

    /// The incoming entry's place among the group's entries.
    fn at(&self, lay: &Layout) -> Option<usize> {
        self.into.map(|(j, s, _)| lay.first[j] + s)
    }
}

/// Where a group's entries sit, the incoming one counted in its leaf:
/// the index of each leaf's first entry and the bytes before it.
struct Layout {
    first: [usize; 4],
    before: [usize; 4],
}

impl Tree {
    fn layout(&self, g: &Group) -> Layout {
        let mut lay = Layout { first: [0; 4], before: [0; 4] };
        for j in 0..3 {
            let (n, b) = match g.into {
                Some((i, _, eb)) if i == j => (1, eb),
                _ => (0, 0),
            };
            let used = if j < g.k { self.leaf(g.ids[j]).used() } else { 0 };
            lay.first[j + 1] = lay.first[j] + g.ns[j] + n;
            lay.before[j + 1] = lay.before[j] + used + b;
        }
        lay
    }

    /// The entry boundary nearest `want` bytes into the group and the
    /// bytes before it, walking in from the nearer end of its leaf.
    fn nearest_cut(&self, g: &Group, lay: &Layout, want: usize) -> (usize, usize) {
        let j = (0..g.k).rfind(|&j| lay.before[j] <= want).expect("before[0] is 0");
        let (lo, hi, blo, bhi) = (lay.first[j], lay.first[j + 1], lay.before[j], lay.before[j + 1]);
        if want >= bhi {
            return (hi, bhi);
        }
        let (l, into) = (self.leaf(g.ids[j]), g.into.filter(|&(i, _, _)| i == j));
        // bytes of the group's entry `i`, which sits in this leaf
        let span = |i: usize| {
            let local = i - lo;
            match into {
                Some((_, s, eb)) if s == local => eb,
                Some((_, s, _)) if s < local => l.span_bytes(local - 1, local),
                _ => l.span_bytes(local, local + 1),
            }
        };
        let (mut i, mut acc) = (lo, blo);
        if want - blo <= bhi - want {
            let mut s = span(i);
            while acc + s <= want {
                (acc, i) = (acc + s, i + 1);
                s = span(i);
            }
            return if want - acc <= acc + s - want { (i, acc) } else { (i + 1, acc + s) };
        }
        (i, acc) = (hi, bhi);
        let mut s = span(i - 1);
        while acc - s >= want {
            (acc, i) = (acc - s, i - 1);
            s = span(i - 1);
        }
        if acc - want <= want - (acc - s) { (i, acc) } else { (i - 1, acc - s) }
    }

    /// Where to cut the group into `m` non-empty parts: each near an even
    /// share, except that when an even share is below [`FILL`] the parts
    /// that are the tree's first or last leaf give way so the others
    /// reach it. `None` when a part would not fit a leaf.
    fn cuts(&self, g: &Group, lay: &Layout, m: usize) -> Option<[usize; 4]> {
        let ends = g.ends(self, m);
        let (n, total) = (lay.first[g.k], lay.before[g.k]);
        let inner = (0..m).filter(|&p| !ends[p]).count();
        let even = total / m;
        let skew = inner > 0 && inner < m && even < FILL;
        let share = if skew { FILL.min(total / inner) } else { even };
        let rest = if skew { (total - share * inner) / (m - inner) } else { even };
        let (mut cuts, mut bytes) = ([0usize; 4], [0usize; 4]);
        (cuts[m], bytes[m]) = (n, total);
        let mut want = 0;
        for j in 1..m {
            want += if ends[j - 1] { rest } else { share };
            (cuts[j], bytes[j]) = self.nearest_cut(g, lay, want);
            if cuts[j] <= cuts[j - 1] || cuts[j] + (m - j) > n {
                return None;
            }
        }
        (0..m).all(|j| bytes[j + 1] - bytes[j] <= Leaf::capacity()).then_some(cuts)
    }

    /// Deal the group's entries, and `e` when there is one, over `m`
    /// leaves by bytes; `false`, changing nothing, when they do not fit.
    pub(crate) fn deal(&mut self, g: &Group, e: Option<Ent<'_>>, m: usize) -> bool {
        let lay = self.layout(g);
        let Some(cuts) = self.cuts(g, &lay, m) else { return false };
        let mut ids = g.ids;
        if m > g.k {
            ids[m - 1] = self.new_leaf();
            self.link_after(g.ids[g.k - 1], ids[m - 1]);
        }
        let at = g.at(&lay);
        let real = |c: usize| c - usize::from(at.is_some_and(|a| a < c));
        let b: [usize; 4] =
            std::array::from_fn(|j| real(if j < m { cuts[j] } else { lay.first[g.k] }));
        self.move_between(ids, g.ns, b);
        let mut got: [usize; 3] = std::array::from_fn(|j| b[j + 1] - b[j]);
        if let (Some(e), Some(at)) = (e, at) {
            let p = (0..m).rfind(|&p| cuts[p] <= at).expect("the first cut is 0");
            let (l, ov) = self.leaf_ov(ids[p]);
            let fit = l.insert_at(at - cuts[p], e, ov);
            debug_assert!(fit, "a dealt leaf holds its share");
            got[p] += 1;
        }
        self.relink(g, ids[m - 1], m, got);
        true
    }

    /// Move entries so leaf `j` of `ids` holds the group's entries
    /// `b[j]..b[j + 1]`, where it now holds `a[j]..a[j + 1]` by `ns`.
    /// The middle leaf gives before it takes and every other leaf only
    /// gives or only takes, so no leaf holds more on the way than at the
    /// start or the end.
    fn move_between(&mut self, ids: [u32; 3], ns: [usize; 3], b: [usize; 4]) {
        let a = [0, ns[0], ns[0] + ns[1], ns[0] + ns[1] + ns[2]];
        let c = |x: usize, y: usize| a[x + 1].min(b[y + 1]).saturating_sub(a[x].max(b[y]));
        // a leaf gives its tail rightward and its head leftward
        for (x, y) in [(1, 2), (1, 0), (0, 2), (0, 1), (2, 0), (2, 1)] {
            let k = c(x, y);
            if k == 0 {
                continue;
            }
            let (src, dst, _) = self.two_leaves(ids[x], ids[y]);
            let from = if x < y { src.len() - k } else { 0 };
            let slot = if x < y { 0 } else { dst.len() };
            src.move_span(from, from + k, dst, slot);
        }
    }

    /// After a deal: fix counts and separators, hang a new last leaf or
    /// drop the emptied ones. Leaf `j` of the group now holds `got[j]`.
    fn relink(&mut self, g: &Group, new: u32, m: usize, got: [usize; 3]) {
        for ((path, &now), &was) in g.paths.iter().zip(&got).zip(&g.ns).take(g.k) {
            self.bump(path, now as i32 - was as i32);
        }
        for j in 1..m.min(g.k) {
            self.boundary(&g.paths[j - 1], &g.paths[j], g.ids[j]);
        }
        let mut last = g.paths[g.k - 1];
        if m > g.k {
            self.bump_above(&last, got[m - 1] as i32);
            let mut sep = Vec::new();
            self.leaf(new).key_into(0, &self.ov, &mut sep);
            self.add_child(&mut last, new, got[m - 1], sep.into_boxed_slice());
            return;
        }
        if m == g.k {
            return;
        }
        // every separator beside a dropped leaf becomes the next leaf's
        // first key, so whichever one the removal keeps still holds
        if let Some((yp, y)) = self.beside(&last, true) {
            self.boundary(&last, &yp, y);
            for j in m..g.k {
                self.boundary(&g.paths[j - 1], &g.paths[j], y);
            }
        }
        for j in (m..g.k).rev() {
            self.unlink(g.ids[j]);
            self.free_leaf(g.ids[j]);
            let mut p = g.paths[j];
            let (parent, at) = p.pop().expect("a dealt leaf has a parent");
            self.remove_child(&mut p, parent, at);
        }
        self.shrink_root();
    }
}
