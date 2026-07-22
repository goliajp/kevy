//! The top-K selection: what a candidate is, what order the page is in,
//! and the little heap that keeps the best of them. A child module of
//! `segment_query` (declared via `#[path]`).

use super::super::TextMatch;

/// The best `limit` candidates seen so far, kept sorted so the worst
/// survivor is always at the end.
pub(super) struct TopK<'a> {
    pub(super) top: Vec<Cand<'a>>,
    pub(super) limit: usize,
    pub(super) order: Order,
}

impl<'a> TopK<'a> {
    pub(super) fn new(limit: usize, order: Order) -> Self {
        Self { top: Vec::with_capacity(limit + 1), limit, order }
    }

    pub(super) fn push(&mut self, cand: Cand<'a>) {
        if self.top.len() < self.limit {
            self.top.push(cand);
            if self.top.len() == self.limit {
                self.order.sort(&mut self.top);
            }
        } else if self.order.better(&cand, &self.top[self.limit - 1]) {
            let pos = self.top.partition_point(|e| self.order.better(e, &cand));
            self.top.insert(pos, cand);
            self.top.pop();
        }
    }

    pub(super) fn finish(mut self) -> Vec<TextMatch> {
        if self.top.len() < self.limit {
            self.order.sort(&mut self.top);
        }
        self.top.into_iter().map(|c| TextMatch { key: c.key.to_vec(), score: c.score }).collect()
    }
}

/// One candidate in the top-K selection.
pub(super) struct Cand<'a> {
    pub(super) score: f64,
    pub(super) key: &'a [u8],
    /// The sort field's order-preserving key, when the query sorts by a
    /// stored value; `None` when it does not, or when this document has
    /// no usable value for the field.
    pub(super) okey: Option<Vec<u8>>,
}

/// What the selection ranks by.
#[derive(Clone, Copy)]
pub(super) struct Order {
    pub(super) sorted: bool,
    pub(super) desc: bool,
}

/// The order a page sorted by a stored value is in: a document WITH a
/// value outranks one without (in both directions — missing is not a
/// value), then by that value's order key, then by row key so ties are
/// stable.
///
/// Public because the cross-shard merge orders the union of the shards'
/// pages and must do it exactly as each shard ordered its own. Two copies
/// of this rule would be two chances to disagree about where the
/// unknowns went.
pub fn sorted_order(
    a: (Option<&[u8]>, &[u8]),
    b: (Option<&[u8]>, &[u8]),
    desc: bool,
) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (a.0, b.0) {
        (Some(x), Some(y)) => {
            let ord = if desc { y.cmp(x) } else { x.cmp(y) };
            ord.then_with(|| a.1.cmp(b.1))
        }
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => a.1.cmp(b.1),
    }
}

// float_cmp: exact equality is the tiebreak trigger — an epsilon here
// would make ranking non-deterministic for genuinely equal BM25 scores.
#[allow(clippy::float_cmp)]
impl Order {
    /// Whether `a` outranks `b`.
    ///
    /// The row key breaks every tie, so two shards holding equally-ranked
    /// documents agree on their order and the merged page is stable.
    /// Under a sort, a document WITH a value always outranks one without,
    /// in both directions.
    pub(super) fn better(self, a: &Cand, b: &Cand) -> bool {
        if !self.sorted {
            return a.score > b.score || (a.score == b.score && a.key < b.key);
        }
        sorted_order((a.okey.as_deref(), a.key), (b.okey.as_deref(), b.key), self.desc)
            == std::cmp::Ordering::Less
    }

    pub(super) fn sort(self, top: &mut [Cand]) {
        top.sort_by(|a, b| {
            if self.better(a, b) {
                std::cmp::Ordering::Less
            } else if self.better(b, a) {
                std::cmp::Ordering::Greater
            } else {
                std::cmp::Ordering::Equal
            }
        });
    }
}
