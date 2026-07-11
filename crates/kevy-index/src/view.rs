//! Views: named composition trees over declared indexes.
//!
//! Pure logic: [`ViewSpec`] (the declaration), [`eval_tree`] (the
//! virtual-mode evaluator over segment closures), and
//! [`MaterializedSet`] (the incremental ordered result set with the
//! bounded top-K discipline). The runtime supplies segment access and
//! wires maintenance to its write hook — nothing here does I/O.
//!
//! Locked structural rules: components are NAMED indexes (leaves carry
//! a shape; the view layer holds no predicates of its own); a view
//! stores MEMBERSHIP + ORDER only (never field values); AND/OR
//! subtrees may be re-ordered by the engine (DIFF is fixed
//! left-right).

use crate::segment::Segment;
use crate::value::IndexValue;

pub use crate::view_sidecar::{MAX_VIEWS, ViewCatalog};

/// One leaf: a declared index + the shape it contributes.
#[derive(Debug, Clone, PartialEq)]
pub struct Leaf {
    /// Index name (resolved by the runtime).
    pub index: Vec<u8>,
    /// Inclusive bounds (EQ = same min/max), already coerced to the
    /// index's type by the runtime at CREATE time.
    pub min: IndexValue,
    /// Upper bound.
    pub max: IndexValue,
}

/// The composition tree. Depth ≤ 3, leaves ≤ 4 (declarative caps,
/// enforced at CREATE).
#[derive(Debug, Clone, PartialEq)]
pub enum Tree {
    /// A single index shape.
    Leaf(Leaf),
    /// Intersection.
    And(Box<Tree>, Box<Tree>),
    /// Union.
    Or(Box<Tree>, Box<Tree>),
    /// Left minus right (NOT commutative — order is fixed).
    Diff(Box<Tree>, Box<Tree>),
}

impl Tree {
    /// Number of leaves.
    pub fn leaves(&self) -> usize {
        match self {
            Tree::Leaf(_) => 1,
            Tree::And(a, b) | Tree::Or(a, b) | Tree::Diff(a, b) => a.leaves() + b.leaves(),
        }
    }

    /// Depth (a leaf is 1).
    pub fn depth(&self) -> usize {
        match self {
            Tree::Leaf(_) => 1,
            Tree::And(a, b) | Tree::Or(a, b) | Tree::Diff(a, b) => 1 + a.depth().max(b.depth()),
        }
    }

    /// Visit every leaf.
    pub fn each_leaf<F: FnMut(&Leaf)>(&self, f: &mut F) {
        match self {
            Tree::Leaf(l) => f(l),
            Tree::And(a, b) | Tree::Or(a, b) | Tree::Diff(a, b) => {
                a.each_leaf(f);
                b.each_leaf(f);
            }
        }
    }
}

/// View mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewMode {
    /// Evaluate the tree at query time.
    Virtual,
    /// Maintain an incremental result set; `top_k = 0` = unbounded.
    Materialized {
        /// Bounded size (0 = keep every member).
        top_k: u32,
    },
}

/// A declared view.
#[derive(Debug, Clone, PartialEq)]
pub struct ViewSpec {
    /// Catalog name.
    pub name: Vec<u8>,
    /// The composition.
    pub tree: Tree,
    /// Index whose coerced value orders the view (a row absent from
    /// this index is excluded — declaratively, counted).
    pub order_by: Vec<u8>,
    /// Descending order?
    pub desc: bool,
    /// Virtual or materialized.
    pub mode: ViewMode,
    /// Optional `VIA` hydration byte-template (`{key}` / `{key.N}`
    /// placeholders; pure dereference, one template hop).
    pub via: Option<Vec<u8>>,
}

/// Declarative caps (RFC §1).
pub const MAX_TREE_DEPTH: usize = 3;
/// Max leaves per tree.
pub const MAX_TREE_LEAVES: usize = 4;

impl ViewSpec {
    /// Validate the structural caps.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.tree.depth() > MAX_TREE_DEPTH {
            return Err("ERR view tree deeper than 3");
        }
        if self.tree.leaves() > MAX_TREE_LEAVES {
            return Err("ERR view tree has more than 4 leaves");
        }
        Ok(())
    }
}

/// Evaluate `tree` against one shard's segments: `seg` resolves an
/// index name to its [`Segment`] (None = unknown index → empty leaf —
/// the runtime validates names at CREATE, so this is defensive).
/// Returns the member keys (unordered set semantics).
pub fn eval_tree<'a>(
    tree: &Tree,
    seg: &impl Fn(&[u8]) -> Option<&'a Segment>,
) -> Vec<Vec<u8>> {
    match tree {
        Tree::Leaf(l) => match seg(&l.index) {
            Some(s) => {
                let (hits, _) = s.range(&l.min, &l.max, None, usize::MAX);
                hits.into_iter().map(|(k, _)| k).collect()
            }
            None => Vec::new(),
        },
        Tree::And(a, b) => {
            // Engine may re-order (locked clause): drive the smaller
            // side, probe the larger.
            let (xa, xb) = (eval_tree(a, seg), eval_tree(b, seg));
            let (mut drive, probe) = if xa.len() <= xb.len() { (xa, xb) } else { (xb, xa) };
            let set: std::collections::HashSet<&[u8]> =
                probe.iter().map(Vec::as_slice).collect();
            drive.retain(|k| set.contains(k.as_slice()));
            drive
        }
        Tree::Or(a, b) => {
            let mut xa = eval_tree(a, seg);
            xa.extend(eval_tree(b, seg));
            xa.sort();
            xa.dedup();
            xa
        }
        Tree::Diff(a, b) => {
            let mut xa = eval_tree(a, seg);
            let xb = eval_tree(b, seg);
            let set: std::collections::HashSet<&[u8]> = xb.iter().map(Vec::as_slice).collect();
            xa.retain(|k| !set.contains(k.as_slice()));
            xa
        }
    }
}

/// Re-evaluate ONE key's membership (the materialized write hook):
/// every leaf is a point probe via the segment's reverse map.
pub fn key_in_tree<'a>(
    tree: &Tree,
    key: &[u8],
    seg: &impl Fn(&[u8]) -> Option<&'a Segment>,
) -> bool {
    match tree {
        Tree::Leaf(l) => seg(&l.index)
            .and_then(|s| s.verify_entry(key))
            .is_some_and(|v| *v >= l.min && *v <= l.max),
        Tree::And(a, b) => key_in_tree(a, key, seg) && key_in_tree(b, key, seg),
        Tree::Or(a, b) => key_in_tree(a, key, seg) || key_in_tree(b, key, seg),
        Tree::Diff(a, b) => key_in_tree(a, key, seg) && !key_in_tree(b, key, seg),
    }
}

/// [`key_in_tree`] variant over PRE-FETCHED per-index values — the
/// write hook probes each referenced index ONCE per key and evaluates
/// every view against the same small table (bounds compares only; no
/// per-view re-hashing).
pub fn key_in_tree_vals(
    tree: &Tree,
    vals: &impl Fn(&[u8]) -> Option<IndexValue>,
) -> bool {
    match tree {
        Tree::Leaf(l) => vals(&l.index).is_some_and(|v| v >= l.min && v <= l.max),
        Tree::And(a, b) => key_in_tree_vals(a, vals) && key_in_tree_vals(b, vals),
        Tree::Or(a, b) => key_in_tree_vals(a, vals) || key_in_tree_vals(b, vals),
        Tree::Diff(a, b) => key_in_tree_vals(a, vals) && !key_in_tree_vals(b, vals),
    }
}

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
    desc: bool,
    /// Members excluded because they're absent from the order index.
    pub order_excluded: u64,
}

impl MaterializedSet {
    /// New set with the declared bound (0 = unbounded) and order
    /// direction (the bound evicts from the view's WORST end).
    pub fn new(top_k: u32, desc: bool) -> Self {
        Self { top_k, desc, ..Default::default() }
    }

    fn cap(&self) -> usize {
        if self.top_k == 0 {
            usize::MAX
        } else {
            (self.top_k + self.top_k / 4) as usize
        }
    }

    /// Apply one key's membership verdict + order value. Returns
    /// `true` if the set UNDERFLOWED below K after a removal (the
    /// caller must schedule a local rebuild).
    pub fn apply(&mut self, key: &[u8], member: bool, order: Option<IndexValue>) -> bool {
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
            let enters = if self.desc {
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
            _ => {
                self.top_k != 0 && self.set.len() < self.top_k as usize
            }
        }
    }

    /// Bound: evict the view's WORST member past K+Δ — the largest
    /// for ASC, the SMALLEST for DESC.
    fn evict_past_cap(&mut self) {
        if self.set.len() > self.cap() {
            let worst = if self.desc {
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

    /// Ordered page. `desc = false`: ascending from just past `after`;
    /// `desc = true`: DESCENDING from just below `after` (a DESC view
    /// must take each shard's LARGEST members — taking the ascending
    /// head and reversing at the merge yields the wrong member set).
    pub fn page(
        &self,
        after: Option<&(IndexValue, Vec<u8>)>,
        limit: usize,
        desc: bool,
    ) -> Vec<(IndexValue, Vec<u8>)> {
        if desc {
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
            Some(c) => Box::new(self.set.range((
                std::ops::Bound::Excluded(c.clone()),
                std::ops::Bound::Unbounded,
            ))),
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
        self.set
            .iter()
            .map(|(v, k)| (v.approx_bytes() + k.len() + 48) as u64)
            .sum()
    }
}

#[cfg(test)]
#[path = "view_tests.rs"]
mod tests;
