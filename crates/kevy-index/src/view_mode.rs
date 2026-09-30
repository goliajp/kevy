//! [`ViewMode`] — how a view answers: evaluated per query, or kept
//! as an incremental result set.

/// View mode.
///
/// ```
/// use kevy_index::{IndexValue, Leaf, Tree, ViewMode, ViewSpec};
/// let t = Tree::Leaf(Leaf::new("age", IndexValue::I64(0), IndexValue::I64(99)));
/// let v = ViewSpec::new("v", t, "age");
/// assert_eq!(v.mode, ViewMode::Virtual, "views are virtual unless declared otherwise");
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ViewMode {
    /// Evaluate the tree at query time.
    ///
    /// ```
    /// use kevy_index::ViewMode;
    /// assert_eq!(ViewMode::Virtual.name(), "virtual");
    /// ```
    Virtual,
    /// Maintain an incremental result set; `top_k = 0` = unbounded.
    ///
    /// ```
    /// use kevy_index::ViewMode;
    /// let m = ViewMode::Materialized { top_k: 10 };
    /// assert_eq!(m.name(), "materialized");
    /// ```
    Materialized {
        /// Bounded size (0 = keep every member).
        ///
        /// ```
        /// use kevy_index::{IndexValue, MaterializedSet, Membership, SortOrder};
        /// // K = 2 keeps K + K/4 = 2 members
        /// let mut top2 = MaterializedSet::new(2, SortOrder::Desc);
        /// for (k, v) in [(b"a", 1), (b"b", 5), (b"c", 3)] {
        ///     top2.apply(k, Membership::Member(Some(IndexValue::I64(v))));
        /// }
        /// assert_eq!(top2.len(), 2, "top_k bounds the members kept");
        /// assert_eq!(top2.page(None, 9)[1], (IndexValue::I64(3), b"c".to_vec()));
        /// ```
        top_k: u32,
    },
}

impl ViewMode {
    /// The mode as `VIEW.CREATE … MODE` and the describe replies spell it.
    ///
    /// ```
    /// use kevy_index::ViewMode;
    /// assert_eq!(ViewMode::Virtual.name(), "virtual");
    /// assert_eq!(ViewMode::Materialized { top_k: 5 }.name(), "materialized");
    /// ```
    pub fn name(&self) -> &'static str {
        match self {
            ViewMode::Virtual => "virtual",
            ViewMode::Materialized { .. } => "materialized",
        }
    }
}
