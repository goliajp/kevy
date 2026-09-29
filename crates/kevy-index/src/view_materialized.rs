//! [`MaterializedSet`] — a materialized view's incremental member set,
//! and the [`Membership`] verdicts that drive it.

use crate::value::IndexValue;
use kevy_text::SortOrder;

/// One shard's materialized result set: ordered `(order_value, key)`
/// members with the bounded top-K discipline (keep `K + Δ` where
/// `Δ = K/4`; underflow requests a local rebuild from the base
/// indexes — RFC §2).
#[derive(Debug, Default)]
pub struct MaterializedSet {
    set: std::collections::BTreeSet<(IndexValue, Vec<u8>)>,
    back: std::collections::HashMap<Vec<u8>, IndexValue>,
    /// 0 = unbounded.
    top_k: u32,
    /// DESC view: the bound keeps the LARGEST members (evict the
    /// smallest past the cap); ASC keeps the smallest.
    order: SortOrder,
    order_excluded: u64,
}

/// One key's standing against a materialized view, as
/// [`MaterializedSet::apply`] takes it.
///
/// ```
/// use kevy_index::{IndexValue, MaterializedSet, Membership, SortOrder};
/// let mut m = MaterializedSet::new(0, SortOrder::Asc);
/// m.apply(b"k1", Membership::Member(Some(IndexValue::I64(1))));
/// m.apply(b"k2", Membership::Member(None));
/// assert_eq!((m.len(), m.order_excluded()), (1, 1));
/// m.apply(b"k1", Membership::NonMember);
/// assert!(m.is_empty());
/// ```
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Membership {
    /// The key matches the view's tree, with its value in the view's
    /// order index — `None` when it has none there, which excludes it
    /// (counted, not an error).
    Member(Option<IndexValue>),
    /// The key does not match the tree, or no longer exists.
    NonMember,
}

impl MaterializedSet {
    /// New set with the declared bound (0 = unbounded) and order
    /// direction (the bound evicts from the view's WORST end).
    ///
    /// ```
    /// use kevy_index::{MaterializedSet, SortOrder};
    /// assert!(MaterializedSet::new(10, SortOrder::Desc).is_empty());
    /// ```
    pub fn new(top_k: u32, order: SortOrder) -> Self {
        Self { top_k, order, ..Default::default() }
    }

    /// Members excluded because they are absent from the order index.
    ///
    /// ```
    /// use kevy_index::{MaterializedSet, Membership, SortOrder};
    /// let mut m = MaterializedSet::new(0, SortOrder::Asc);
    /// m.apply(b"k", Membership::Member(None));
    /// assert_eq!(m.order_excluded(), 1);
    /// ```
    pub fn order_excluded(&self) -> u64 {
        self.order_excluded
    }

    fn cap(&self) -> usize {
        if self.top_k == 0 { usize::MAX } else { (self.top_k + self.top_k / 4) as usize }
    }

    /// Apply one key's membership verdict + order value. Returns
    /// `true` if the set UNDERFLOWED below K after a removal (the
    /// caller must schedule a local rebuild).
    pub fn apply(&mut self, key: &[u8], membership: Membership) -> bool {
        let (member, order) = match membership {
            Membership::Member(order) => (true, order),
            Membership::NonMember => (false, None),
        };
        // Bounded fast path: a NON-member of a full top-K set whose
        // value is worse than the current worst can neither enter nor
        // change anything — one comparison, no tree ops, no allocs.
        // This is the write-tax fast path for hot-list views (most
        // writes touch rows outside the top K).
        if self.top_k != 0
            && member
            && !self.back.contains_key(key)
            && self.set.len() >= self.cap()
            && let Some(v) = &order
        {
            let enters = if self.order == SortOrder::Desc {
                self.set.iter().next().is_some_and(|(worst, _)| v > worst)
            } else {
                self.set.iter().next_back().is_some_and(|(worst, _)| v < worst)
            };
            if !enters {
                return false;
            }
        }
        if let Some(old) = self.back.remove(key) {
            self.set.remove(&(old, key.to_vec()));
        }
        match (member, order) {
            (true, Some(v)) => {
                self.back.insert(key.to_vec(), v.clone());
                self.set.insert((v, key.to_vec()));
                self.evict_past_cap();
                false
            }
            (true, None) => {
                self.order_excluded += 1;
                false
            }
            _ => self.top_k != 0 && self.set.len() < self.top_k as usize,
        }
    }

    /// Bound: evict the view's WORST member past K+Δ — the largest
    /// for ASC, the SMALLEST for DESC.
    fn evict_past_cap(&mut self) {
        if self.set.len() > self.cap() {
            let worst = if self.order == SortOrder::Desc {
                self.set.iter().next().cloned()
            } else {
                self.set.iter().next_back().cloned()
            };
            if let Some(w) = worst {
                self.set.remove(&w);
                self.back.remove(&w.1);
            }
        }
    }

    /// Ordered page in the set's own direction: ascending from just past
    /// `after`, or DESCENDING from just below it (a DESC view must take
    /// each shard's LARGEST members — taking the ascending head and
    /// reversing at the merge yields the wrong member set).
    ///
    /// ```
    /// use kevy_index::{IndexValue, MaterializedSet, Membership, SortOrder};
    /// let mut m = MaterializedSet::new(0, SortOrder::Desc);
    /// for (k, v) in [(b"a", 1), (b"b", 2)] {
    ///     m.apply(k, Membership::Member(Some(IndexValue::I64(v))));
    /// }
    /// assert_eq!(m.page(None, 1), vec![(IndexValue::I64(2), b"b".to_vec())]);
    /// ```
    pub fn page(
        &self,
        after: Option<&(IndexValue, Vec<u8>)>,
        limit: usize,
    ) -> Vec<(IndexValue, Vec<u8>)> {
        if self.order == SortOrder::Desc {
            let iter: Box<dyn Iterator<Item = &(IndexValue, Vec<u8>)>> = match after {
                Some(c) => Box::new(
                    self.set
                        .range((std::ops::Bound::Unbounded, std::ops::Bound::Excluded(c.clone())))
                        .rev(),
                ),
                None => Box::new(self.set.iter().rev()),
            };
            return iter.take(limit).cloned().collect();
        }
        let iter: Box<dyn Iterator<Item = &(IndexValue, Vec<u8>)>> = match after {
            Some(c) => Box::new(
                self.set.range((std::ops::Bound::Excluded(c.clone()), std::ops::Bound::Unbounded)),
            ),
            None => Box::new(self.set.iter()),
        };
        iter.take(limit).cloned().collect()
    }

    /// Member count.
    pub fn len(&self) -> usize {
        self.set.len()
    }

    /// Empty?
    pub fn is_empty(&self) -> bool {
        self.set.is_empty()
    }

    /// Wipe (rebuild path).
    pub fn clear(&mut self) {
        self.set.clear();
        self.back.clear();
    }

    /// Approximate heap bytes (RFC §5 formula's measured side).
    pub fn approx_bytes(&self) -> u64 {
        self.set.iter().map(|(v, k)| (v.approx_bytes() + k.len() + 48) as u64).sum()
    }
}
