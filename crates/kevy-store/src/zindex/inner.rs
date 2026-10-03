//! An inner node: up to [`INNER_CAP`] children, the separators between
//! them, and how many entries each child holds — what turns a descent
//! into a rank.
//!
//! `seps[i]` (with `sep_members[i]`) is a lower bound of child `i + 1`:
//! no greater than any of its entries, greater than every entry of child
//! `i`. Separator scores come first so a search reads them alone; the
//! members are read only where two scores are equal.

use core::cmp::Ordering;
use core::mem;

use super::Node;
use crate::value::SmallBytes;

/// Children per inner node.
pub(super) const INNER_CAP: usize = 32;
/// Fewest children an inner node other than the root keeps.
pub(super) const INNER_MIN: usize = INNER_CAP / 2;

/// A separator: a score key and a member.
pub(super) type Sep = (u64, SmallBytes);

#[derive(Clone, Debug)]
#[repr(C)]
pub(crate) struct Inner {
    len: usize,
    seps: [u64; INNER_CAP - 1],
    counts: [usize; INNER_CAP],
    kids: [Option<Node>; INNER_CAP],
    sep_members: [SmallBytes; INNER_CAP - 1],
}

impl Inner {
    /// A node over one child holding `count` entries.
    pub(super) fn single(kid: Node, count: usize) -> Self {
        let mut n = Self::empty();
        n.kids[0] = Some(kid);
        n.counts[0] = count;
        n.len = 1;
        n
    }

    /// A node over two children, `sep` between them.
    pub(super) fn pair(left: (Node, usize), sep: Sep, right: (Node, usize)) -> Self {
        let mut n = Self::single(left.0, left.1);
        n.push_back(sep, right.0, right.1);
        n
    }

    fn empty() -> Self {
        Self {
            len: 0,
            seps: [0; INNER_CAP - 1],
            counts: [0; INNER_CAP],
            kids: [const { None }; INNER_CAP],
            sep_members: [const { SmallBytes::new() }; INNER_CAP - 1],
        }
    }

    pub(super) fn len(&self) -> usize {
        self.len
    }

    pub(super) fn count(&self, i: usize) -> usize {
        self.counts[i]
    }

    pub(super) fn count_mut(&mut self, i: usize) -> &mut usize {
        &mut self.counts[i]
    }

    /// Entries under the children before `i`.
    pub(super) fn before(&self, i: usize) -> usize {
        self.counts[..i].iter().sum()
    }

    pub(super) fn total(&self) -> usize {
        self.before(self.len)
    }

    pub(super) fn kid(&self, i: usize) -> &Node {
        self.kids[i].as_ref().expect("a child below the count")
    }

    pub(super) fn kid_mut(&mut self, i: usize) -> &mut Node {
        self.kids[i].as_mut().expect("a child below the count")
    }

    /// Children `a` and `a + 1`, both writable.
    pub(super) fn kid_pair(&mut self, a: usize) -> (&mut Node, &mut Node) {
        let (l, r) = self.kids.split_at_mut(a + 1);
        let live = "a child below the count";
        (l[a].as_mut().expect(live), r[0].as_mut().expect(live))
    }

    pub(super) fn sep(&self, i: usize) -> Sep {
        (self.seps[i], self.sep_members[i].clone())
    }

    pub(super) fn set_sep(&mut self, i: usize, sep: Sep) {
        self.seps[i] = sep.0;
        self.sep_members[i] = sep.1;
    }

    /// The child `(sk, m)` belongs under: how many separators are at or
    /// below it.
    pub(super) fn route(&self, sk: u64, m: &[u8]) -> usize {
        let (mut lo, mut hi) = (0, self.len - 1);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let at_or_below = match self.seps[mid].cmp(&sk) {
                Ordering::Less => true,
                Ordering::Greater => false,
                Ordering::Equal => self.sep_members[mid].as_slice() <= m,
            };
            if at_or_below {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo
    }

    /// The child where a score predicate, true on a prefix, stops
    /// holding: every child before it holds only entries it is true for,
    /// every child after it only entries it is false for.
    pub(super) fn partition(&self, pred: &impl Fn(u64) -> bool) -> usize {
        let (mut lo, mut hi) = (0, self.len - 1);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if pred(self.seps[mid]) {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo
    }

    /// [`Self::partition`] for a predicate on whole keys.
    pub(super) fn partition_keys(&self, pred: &impl Fn(u64, &[u8]) -> bool) -> usize {
        let (mut lo, mut hi) = (0, self.len - 1);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if pred(self.seps[mid], self.sep_members[mid].as_slice()) {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo
    }

    /// Put `kid` holding `count` entries right after child `i`, `sep`
    /// between them. The node must have room.
    pub(super) fn insert_after(&mut self, i: usize, sep: Sep, kid: Node, count: usize) {
        let n = self.len;
        self.kids[i + 1..=n].rotate_right(1);
        self.kids[i + 1] = Some(kid);
        self.counts[i + 1..=n].rotate_right(1);
        self.counts[i + 1] = count;
        self.seps[i..n].rotate_right(1);
        self.sep_members[i..n].rotate_right(1);
        self.set_sep(i, sep);
        self.len = n + 1;
    }

    /// Take out child `i` (not the first) and the separator before it.
    pub(super) fn remove(&mut self, i: usize) -> (Sep, Node, usize) {
        let n = self.len;
        let sep = (self.seps[i - 1], mem::replace(&mut self.sep_members[i - 1], SmallBytes::new()));
        self.seps[i - 1..n - 1].rotate_left(1);
        self.sep_members[i - 1..n - 1].rotate_left(1);
        let kid = self.kids[i].take().expect("a child below the count");
        self.kids[i..n].rotate_left(1);
        let count = self.counts[i];
        self.counts[i..n].rotate_left(1);
        self.len = n - 1;
        (sep, kid, count)
    }

    /// Add `kid` as the last child, `sep` before it.
    pub(super) fn push_back(&mut self, sep: Sep, kid: Node, count: usize) {
        self.insert_after(self.len - 1, sep, kid, count);
    }

    /// Take out the first child and the separator after it.
    pub(super) fn pop_front(&mut self) -> (Sep, Node, usize) {
        let (sep, second, second_count) = self.remove(1);
        let first = mem::replace(self.kid_mut(0), second);
        let first_count = mem::replace(&mut self.counts[0], second_count);
        (sep, first, first_count)
    }

    /// Add `kid` as the first child, `sep` after it.
    pub(super) fn push_front(&mut self, sep: Sep, kid: Node, count: usize) {
        let old = mem::replace(self.kid_mut(0), kid);
        let old_count = mem::replace(&mut self.counts[0], count);
        self.insert_after(0, sep, old, old_count);
    }

    /// Take out the last child and the separator before it.
    pub(super) fn pop_back(&mut self) -> (Sep, Node, usize) {
        self.remove(self.len - 1)
    }

    /// Move children `at..` into a new node; the separator before child
    /// `at`, which neither half keeps, comes back with it.
    pub(super) fn split_off(&mut self, at: usize) -> (Sep, Inner) {
        let n = self.len;
        let up =
            (self.seps[at - 1], mem::replace(&mut self.sep_members[at - 1], SmallBytes::new()));
        let first = self.kids[at].take().expect("a child below the count");
        let mut right = Self::single(first, mem::take(&mut self.counts[at]));
        for i in at + 1..n {
            let sep =
                (self.seps[i - 1], mem::replace(&mut self.sep_members[i - 1], SmallBytes::new()));
            let kid = self.kids[i].take().expect("a child below the count");
            right.push_back(sep, kid, mem::take(&mut self.counts[i]));
        }
        self.len = at;
        (up, right)
    }

    /// Move every child of `right`, all above this node's, onto its end,
    /// `sep` between the two.
    pub(super) fn append(&mut self, sep: Sep, mut right: Inner) {
        let n = right.len;
        let first = right.kids[0].take().expect("a child below the count");
        self.push_back(sep, first, right.counts[0]);
        for i in 1..n {
            let s =
                (right.seps[i - 1], mem::replace(&mut right.sep_members[i - 1], SmallBytes::new()));
            let kid = right.kids[i].take().expect("a child below the count");
            self.push_back(s, kid, right.counts[i]);
        }
    }
}
