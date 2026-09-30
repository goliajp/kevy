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

use kevy_text::SortOrder;

pub use crate::view_materialized::{MaterializedSet, Membership};
pub use crate::view_mode::ViewMode;
pub use crate::view_sidecar::{MAX_VIEWS, ViewCatalog};

/// One leaf: a declared index + the shape it contributes.
///
/// ```
/// use kevy_index::{IndexValue, Leaf};
/// let adults = Leaf::new("age", IndexValue::I64(18), IndexValue::I64(i64::MAX));
/// assert_eq!(adults.index, b"age");
/// ```
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct Leaf {
    /// Index name (resolved by the runtime).
    ///
    /// ```
    /// use kevy_index::{IndexValue, Leaf};
    /// let leaf = Leaf::new("city", IndexValue::Str(b"a".to_vec()), IndexValue::Str(b"m".to_vec()));
    /// assert_eq!(leaf.index, b"city");
    /// ```
    pub index: Vec<u8>,
    /// Inclusive bounds (EQ = same min/max), already coerced to the
    /// index's type by the runtime at CREATE time.
    ///
    /// ```
    /// use kevy_index::{IndexValue, Leaf, Tree};
    /// let leaf = Leaf::new("age", IndexValue::I64(18), IndexValue::I64(65));
    /// assert_eq!(leaf.min, IndexValue::I64(18));
    /// let t = Tree::Leaf(leaf);
    /// assert!(t.contains_values(&|_: &[u8]| Some(IndexValue::I64(18))), "min is inclusive");
    /// assert!(!t.contains_values(&|_: &[u8]| Some(IndexValue::I64(17))));
    /// ```
    pub min: IndexValue,
    /// Upper bound.
    ///
    /// ```
    /// use kevy_index::{IndexValue, Leaf, Tree};
    /// let leaf = Leaf::new("age", IndexValue::I64(18), IndexValue::I64(65));
    /// assert_eq!(leaf.max, IndexValue::I64(65));
    /// let t = Tree::Leaf(leaf);
    /// assert!(t.contains_values(&|_: &[u8]| Some(IndexValue::I64(65))), "max is inclusive");
    /// assert!(!t.contains_values(&|_: &[u8]| Some(IndexValue::I64(66))));
    /// ```
    pub max: IndexValue,
}

impl Leaf {
    /// Rows of index `index` whose value lies in `min..=max` (EQ is
    /// `min == max`), bounds already coerced to the index's type.
    ///
    /// ```
    /// use kevy_index::{IndexValue, Leaf};
    /// let eq = Leaf::new("city", IndexValue::Str(b"kyoto".to_vec()), IndexValue::Str(b"kyoto".to_vec()));
    /// assert_eq!(eq.min, eq.max);
    /// ```
    pub fn new(index: impl Into<Vec<u8>>, min: IndexValue, max: IndexValue) -> Leaf {
        Leaf { index: index.into(), min, max }
    }
}

/// The composition tree. Depth ≤ 3, leaves ≤ 4 (declarative caps,
/// enforced at CREATE).
///
/// ```
/// use kevy_index::{IndexValue, Leaf, Segment, Tree};
///
/// let mut age = Segment::new();
/// age.set_key_dir(true);
/// age.apply(b"u:1", None, Some(IndexValue::I64(30)));
/// age.apply(b"u:2", None, Some(IndexValue::I64(70)));
/// let mut city = Segment::new();
/// city.set_key_dir(true);
/// city.apply(b"u:1", None, Some(IndexValue::Str(b"kyoto".to_vec())));
/// city.apply(b"u:2", None, Some(IndexValue::Str(b"kyoto".to_vec())));
///
/// let kyoto = IndexValue::Str(b"kyoto".to_vec());
/// let t = Tree::And(
///     Box::new(Tree::Leaf(Leaf::new("city", kyoto.clone(), kyoto))),
///     Box::new(Tree::Leaf(Leaf::new("age", IndexValue::I64(18), IndexValue::I64(65)))),
/// );
/// let seg = |name: &[u8]| match name {
///     b"age" => Some(&age),
///     b"city" => Some(&city),
///     _ => None,
/// };
/// assert_eq!(t.eval(&seg), vec![b"u:1".to_vec()]);
/// assert!(t.contains(b"u:1", &seg) && !t.contains(b"u:2", &seg));
/// assert_eq!((t.leaves(), t.depth()), (2, 2));
///
/// let mut names = Vec::new();
/// t.each_leaf(&mut |l: &Leaf| names.push(l.index.clone()));
/// assert_eq!(names, [b"city".to_vec(), b"age".to_vec()]);
/// ```
#[derive(Debug, Clone, PartialEq)]
pub enum Tree {
    /// A single index shape.
    ///
    /// ```
    /// use kevy_index::{IndexValue, Leaf, Tree};
    /// let t = Tree::Leaf(Leaf::new("age", IndexValue::I64(0), IndexValue::I64(9)));
    /// assert_eq!((t.leaves(), t.depth()), (1, 1));
    /// ```
    Leaf(Leaf),
    /// Intersection.
    ///
    /// ```
    /// use kevy_index::{IndexValue, Leaf, Tree};
    /// let leaf = |lo, hi| Box::new(Tree::Leaf(Leaf::new("n", IndexValue::I64(lo), IndexValue::I64(hi))));
    /// let t = Tree::And(leaf(0, 10), leaf(5, 20));
    /// assert!(t.contains_values(&|_: &[u8]| Some(IndexValue::I64(7))));
    /// assert!(!t.contains_values(&|_: &[u8]| Some(IndexValue::I64(3))));
    /// ```
    And(Box<Tree>, Box<Tree>),
    /// Union.
    ///
    /// ```
    /// use kevy_index::{IndexValue, Leaf, Tree};
    /// let leaf = |lo, hi| Box::new(Tree::Leaf(Leaf::new("n", IndexValue::I64(lo), IndexValue::I64(hi))));
    /// let t = Tree::Or(leaf(0, 1), leaf(8, 9));
    /// assert!(t.contains_values(&|_: &[u8]| Some(IndexValue::I64(9))));
    /// assert!(!t.contains_values(&|_: &[u8]| Some(IndexValue::I64(5))));
    /// ```
    Or(Box<Tree>, Box<Tree>),
    /// Left minus right (NOT commutative — order is fixed).
    ///
    /// ```
    /// use kevy_index::{IndexValue, Leaf, Tree};
    /// let leaf = |lo, hi| Box::new(Tree::Leaf(Leaf::new("n", IndexValue::I64(lo), IndexValue::I64(hi))));
    /// let t = Tree::Diff(leaf(0, 10), leaf(5, 10));
    /// assert!(t.contains_values(&|_: &[u8]| Some(IndexValue::I64(4))));
    /// assert!(!t.contains_values(&|_: &[u8]| Some(IndexValue::I64(6))));
    /// ```
    Diff(Box<Tree>, Box<Tree>),
}

impl Tree {
    /// Evaluate `tree` against one shard's segments: `seg` resolves an
    /// index name to its [`Segment`] (None = unknown index → empty leaf —
    /// the runtime validates names at CREATE, so this is defensive).
    /// Returns the member keys (unordered set semantics).
    ///
    /// ```
    /// use kevy_index::{IndexValue, Leaf, Segment, Tree};
    /// let mut age = Segment::new();
    /// age.set_key_dir(true);
    /// age.apply(b"u:1", None, Some(IndexValue::I64(30)));
    /// age.apply(b"u:2", None, Some(IndexValue::I64(70)));
    /// let t = Tree::Leaf(Leaf::new("age", IndexValue::I64(18), IndexValue::I64(65)));
    /// let seg = |name: &[u8]| (name == b"age").then_some(&age);
    /// assert_eq!(t.eval(&seg), vec![b"u:1".to_vec()]);
    /// assert!(t.contains(b"u:1", &seg) && !t.contains(b"u:2", &seg));
    /// ```
    pub fn eval<'a>(&self, seg: &impl Fn(&[u8]) -> Option<&'a Segment>) -> Vec<Vec<u8>> {
        eval_tree(self, seg)
    }

    /// Re-evaluate ONE key's membership (the materialized write hook):
    /// every leaf is a point probe in the segment's key directory
    /// ([`Segment::set_key_dir`]); a segment without one holds no key.
    pub fn contains<'a>(&self, key: &[u8], seg: &impl Fn(&[u8]) -> Option<&'a Segment>) -> bool {
        key_in_tree(self, key, seg)
    }

    /// [`Tree::contains`] variant over PRE-FETCHED per-index values — the
    /// write hook probes each referenced index ONCE per key and evaluates
    /// every view against the same small table (bounds compares only; no
    /// per-view re-hashing).
    ///
    /// ```
    /// use kevy_index::{IndexValue, Leaf, Tree};
    /// let t = Tree::Leaf(Leaf::new("age", IndexValue::I64(18), IndexValue::I64(65)));
    /// assert!(t.contains_values(&|_: &[u8]| Some(IndexValue::I64(40))));
    /// assert!(!t.contains_values(&|_: &[u8]| None));
    /// ```
    pub fn contains_values(&self, vals: &impl Fn(&[u8]) -> Option<IndexValue>) -> bool {
        key_in_tree_vals(self, vals)
    }

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

/// A declared view.
///
/// ```
/// use kevy_index::{IndexValue, Leaf, SortOrder, Tree, ViewMode, ViewSpec};
/// let leaf = Leaf::new("age", IndexValue::I64(18), IndexValue::I64(65));
/// let v = ViewSpec::new("adults", Tree::Leaf(leaf), "age")
///     .with_order(SortOrder::Desc)
///     .with_mode(ViewMode::Materialized { top_k: 100 });
/// assert!(v.validate().is_ok());
/// ```
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct ViewSpec {
    /// Catalog name.
    ///
    /// ```
    /// use kevy_index::{IndexValue, Leaf, Tree, ViewCatalog, ViewSpec};
    /// let t = Tree::Leaf(Leaf::new("age", IndexValue::I64(0), IndexValue::I64(99)));
    /// let mut views = ViewCatalog::new();
    /// views.create(ViewSpec::new("adults", t, "age"))?;
    /// assert_eq!(views.get(b"adults").map(|v| v.name.clone()), Some(b"adults".to_vec()));
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub name: Vec<u8>,
    /// The composition.
    ///
    /// ```
    /// use kevy_index::{IndexValue, Leaf, Tree, ViewSpec};
    /// let t = Tree::Leaf(Leaf::new("age", IndexValue::I64(0), IndexValue::I64(99)));
    /// let v = ViewSpec::new("v", t.clone(), "age");
    /// assert_eq!(v.tree, t);
    /// assert_eq!(v.tree.leaves(), 1);
    /// ```
    pub tree: Tree,
    /// Index whose coerced value orders the view (a row absent from
    /// this index is excluded — declaratively, counted).
    ///
    /// ```
    /// use kevy_index::{IndexValue, Leaf, Tree, ViewSpec};
    /// let city = |c: &[u8]| IndexValue::Str(c.to_vec());
    /// let t = Tree::Leaf(Leaf::new("city", city(b"a"), city(b"z")));
    /// let v = ViewSpec::new("by_age", t, "age");
    /// assert_eq!(v.order_by, b"age", "the order index need not be in the tree");
    /// ```
    pub order_by: Vec<u8>,
    /// Direction of that order.
    ///
    /// ```
    /// use kevy_index::{IndexValue, Leaf, SortOrder, Tree, ViewSpec};
    /// let t = Tree::Leaf(Leaf::new("age", IndexValue::I64(0), IndexValue::I64(99)));
    /// let v = ViewSpec::new("v", t, "age");
    /// assert_eq!(v.order, SortOrder::Asc);
    /// assert_eq!(v.with_order(SortOrder::Desc).order, SortOrder::Desc);
    /// ```
    pub order: SortOrder,
    /// Virtual or materialized.
    ///
    /// ```
    /// use kevy_index::{IndexValue, Leaf, Tree, ViewMode, ViewSpec};
    /// let t = Tree::Leaf(Leaf::new("age", IndexValue::I64(0), IndexValue::I64(99)));
    /// let v = ViewSpec::new("v", t, "age").with_mode(ViewMode::Materialized { top_k: 50 });
    /// assert_eq!(v.mode.name(), "materialized");
    /// ```
    pub mode: ViewMode,
    /// Optional `VIA` hydration byte-template (`{key}` / `{key.N}`
    /// placeholders; pure dereference, one template hop).
    ///
    /// ```
    /// use kevy_index::{IndexValue, Leaf, Tree, ViewSpec};
    /// let t = Tree::Leaf(Leaf::new("age", IndexValue::I64(0), IndexValue::I64(99)));
    /// let v = ViewSpec::new("v", t, "age");
    /// assert_eq!(v.via, None, "members are returned as keys unless VIA is declared");
    /// ```
    pub via: Option<Vec<u8>>,
}

/// Declarative caps (RFC §1).
///
/// ```
/// use kevy_index::{IndexValue, Leaf, MAX_TREE_DEPTH, Tree, ViewError, ViewSpec};
/// let leaf = || Box::new(Tree::Leaf(Leaf::new("n", IndexValue::I64(0), IndexValue::I64(1))));
/// // And(And(Or(leaf, leaf), leaf), leaf) is four levels deep
/// let deep = Tree::And(Box::new(Tree::And(Box::new(Tree::Or(leaf(), leaf())), leaf())), leaf());
/// assert_eq!(deep.depth(), MAX_TREE_DEPTH + 1);
/// assert_eq!(ViewSpec::new("v", deep, "n").validate(), Err(ViewError::TooDeep));
/// ```
pub const MAX_TREE_DEPTH: usize = 3;
/// Max leaves per tree.
///
/// ```
/// use kevy_index::{IndexValue, Leaf, MAX_TREE_LEAVES, Tree, ViewSpec};
/// let leaf = || Box::new(Tree::Leaf(Leaf::new("n", IndexValue::I64(0), IndexValue::I64(1))));
/// let pair = || Box::new(Tree::Or(leaf(), leaf()));
/// let full = Tree::And(pair(), pair());
/// assert_eq!(full.leaves(), MAX_TREE_LEAVES);
/// assert!(ViewSpec::new("v", full, "n").validate().is_ok());
///
/// let wide = Tree::Or(Box::new(Tree::Or(pair(), pair())), leaf());
/// assert_eq!(wide.leaves(), MAX_TREE_LEAVES + 1);
/// assert!(ViewSpec::new("v", wide, "n").validate().is_err());
/// ```
pub const MAX_TREE_LEAVES: usize = 4;

impl ViewSpec {
    /// A virtual view `name` over `tree`, ascending by index `order_by`.
    ///
    /// ```
    /// use kevy_index::{IndexValue, Leaf, SortOrder, Tree, ViewMode, ViewSpec};
    /// let v = ViewSpec::new("v", Tree::Leaf(Leaf::new("a", IndexValue::I64(0), IndexValue::I64(9))), "a");
    /// assert_eq!((v.order, v.mode, v.via), (SortOrder::Asc, ViewMode::Virtual, None));
    /// ```
    pub fn new(name: impl Into<Vec<u8>>, tree: Tree, order_by: impl Into<Vec<u8>>) -> ViewSpec {
        ViewSpec {
            name: name.into(),
            tree,
            order_by: order_by.into(),
            order: SortOrder::Asc,
            mode: ViewMode::Virtual,
            via: None,
        }
    }

    /// This view ordered in `order`.
    ///
    /// ```
    /// # use kevy_index::{IndexValue, Leaf, SortOrder, Tree, ViewSpec};
    /// # let t = Tree::Leaf(Leaf::new("a", IndexValue::I64(0), IndexValue::I64(9)));
    /// assert_eq!(ViewSpec::new("v", t, "a").with_order(SortOrder::Desc).order, SortOrder::Desc);
    /// ```
    #[must_use]
    pub fn with_order(mut self, order: SortOrder) -> ViewSpec {
        self.order = order;
        self
    }

    /// This view evaluated as `mode`.
    ///
    /// ```
    /// # use kevy_index::{IndexValue, Leaf, Tree, ViewMode, ViewSpec};
    /// # let t = Tree::Leaf(Leaf::new("a", IndexValue::I64(0), IndexValue::I64(9)));
    /// let m = ViewMode::Materialized { top_k: 0 };
    /// assert_eq!(ViewSpec::new("v", t, "a").with_mode(m).mode, m);
    /// ```
    #[must_use]
    pub fn with_mode(mut self, mode: ViewMode) -> ViewSpec {
        self.mode = mode;
        self
    }

    /// This view hydrated through the `VIA` byte-template `via`.
    ///
    /// ```
    /// # use kevy_index::{IndexValue, Leaf, Tree, ViewSpec};
    /// # let t = Tree::Leaf(Leaf::new("a", IndexValue::I64(0), IndexValue::I64(9)));
    /// let v = ViewSpec::new("v", t, "a").with_via(b"doc:{key}".to_vec());
    /// assert_eq!(v.via.as_deref(), Some(&b"doc:{key}"[..]));
    /// ```
    #[must_use]
    pub fn with_via(mut self, via: Vec<u8>) -> ViewSpec {
        self.via = Some(via);
        self
    }

    /// Validate the structural caps.
    pub fn validate(&self) -> Result<(), crate::ViewError> {
        if self.tree.depth() > MAX_TREE_DEPTH {
            return Err(crate::ViewError::TooDeep);
        }
        if self.tree.leaves() > MAX_TREE_LEAVES {
            return Err(crate::ViewError::TooManyLeaves);
        }
        Ok(())
    }
}

pub(crate) fn eval_tree<'a>(
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
            let (xa, xb) = (a.eval(seg), b.eval(seg));
            let (mut drive, probe) = if xa.len() <= xb.len() { (xa, xb) } else { (xb, xa) };
            let set: std::collections::HashSet<&[u8]> = probe.iter().map(Vec::as_slice).collect();
            drive.retain(|k| set.contains(k.as_slice()));
            drive
        }
        Tree::Or(a, b) => {
            let mut xa = a.eval(seg);
            xa.extend(b.eval(seg));
            xa.sort();
            xa.dedup();
            xa
        }
        Tree::Diff(a, b) => {
            let mut xa = a.eval(seg);
            let xb = b.eval(seg);
            let set: std::collections::HashSet<&[u8]> = xb.iter().map(Vec::as_slice).collect();
            xa.retain(|k| !set.contains(k.as_slice()));
            xa
        }
    }
}

pub(crate) fn key_in_tree<'a>(
    tree: &Tree,
    key: &[u8],
    seg: &impl Fn(&[u8]) -> Option<&'a Segment>,
) -> bool {
    match tree {
        Tree::Leaf(l) => seg(&l.index)
            .and_then(|s| s.key_dir()?.get(key))
            .is_some_and(|v| v >= l.min && v <= l.max),
        Tree::And(a, b) => a.contains(key, seg) && b.contains(key, seg),
        Tree::Or(a, b) => a.contains(key, seg) || b.contains(key, seg),
        Tree::Diff(a, b) => a.contains(key, seg) && !b.contains(key, seg),
    }
}

pub(crate) fn key_in_tree_vals(tree: &Tree, vals: &impl Fn(&[u8]) -> Option<IndexValue>) -> bool {
    match tree {
        Tree::Leaf(l) => vals(&l.index).is_some_and(|v| v >= l.min && v <= l.max),
        Tree::And(a, b) => a.contains_values(vals) && b.contains_values(vals),
        Tree::Or(a, b) => a.contains_values(vals) || b.contains_values(vals),
        Tree::Diff(a, b) => a.contains_values(vals) && !b.contains_values(vals),
    }
}

#[cfg(test)]
#[path = "view_tests.rs"]
mod tests;
