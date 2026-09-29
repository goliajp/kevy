//! Named arguments for the command flags the store takes: write
//! conditions, list ends and positions, and score comparisons.

/// When a write may happen, judged by whether its target already exists:
/// `SET`'s and `ZADD`'s `NX` / `XX`. One value instead of two flags, so
/// asking for both is not expressible.
///
/// ```
/// use kevy_store::{SetCondition, Store};
/// let mut s = Store::new();
/// assert!(!s.set(b"k", b"1".to_vec(), None, SetCondition::IfPresent));
/// assert!(s.set(b"k", b"1".to_vec(), None, SetCondition::IfAbsent));
/// assert!(!s.set(b"k", b"2".to_vec(), None, SetCondition::IfAbsent));
/// assert!(s.set(b"k", b"2".to_vec(), None, SetCondition::Always));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum SetCondition {
    /// Write whether or not the target exists.
    ///
    /// ```
    /// use kevy_store::{SetCondition, Store};
    /// let mut s = Store::new();
    /// assert!(s.set(b"k", b"1".to_vec(), None, SetCondition::Always));
    /// assert!(s.set(b"k", b"2".to_vec(), None, SetCondition::Always));
    /// assert_eq!(s.get(b"k")?.as_deref(), Some(&b"2"[..]));
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    #[default]
    Always,
    /// `NX`: write only when the target does not exist.
    ///
    /// ```
    /// use kevy_store::{SetCondition, Store};
    /// let mut s = Store::new();
    /// assert!(s.set(b"k", b"1".to_vec(), None, SetCondition::IfAbsent));
    /// assert!(!s.set(b"k", b"2".to_vec(), None, SetCondition::IfAbsent));
    /// assert_eq!(s.get(b"k")?.as_deref(), Some(&b"1"[..]));
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    IfAbsent,
    /// `XX`: write only when the target already exists.
    ///
    /// ```
    /// use kevy_store::{SetCondition, Store};
    /// let mut s = Store::new();
    /// assert!(!s.set(b"k", b"1".to_vec(), None, SetCondition::IfPresent));
    /// assert_eq!(s.get(b"k")?, None);
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    IfPresent,
}

/// One end of a list: `LEFT` (the head, index 0) or `RIGHT` (the tail).
///
/// ```
/// use kevy_store::{ListEnd, Store};
/// let mut s = Store::new();
/// s.rpush(b"src", &[b"a".as_slice(), b"b".as_slice()]).unwrap();
/// let moved = s.lmove(b"src", b"dst", ListEnd::Right, ListEnd::Left).unwrap();
/// assert_eq!(moved.as_deref(), Some(&b"b"[..]));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ListEnd {
    /// The head: `LPUSH` / `LPOP` side.
    ///
    /// ```
    /// use kevy_store::{ListEnd, Store};
    /// let mut s = Store::new();
    /// s.rpush(b"src", &[b"a".as_slice(), b"b".as_slice()])?;
    /// let moved = s.lmove(b"src", b"dst", ListEnd::Left, ListEnd::Left)?;
    /// assert_eq!(moved.as_deref(), Some(&b"a"[..]));
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    Left,
    /// The tail: `RPUSH` / `RPOP` side.
    ///
    /// ```
    /// use kevy_store::{ListEnd, Store};
    /// let mut s = Store::new();
    /// s.rpush(b"src", &[b"a".as_slice(), b"b".as_slice()])?;
    /// s.lmove(b"src", b"dst", ListEnd::Right, ListEnd::Right)?;
    /// s.lmove(b"src", b"dst", ListEnd::Right, ListEnd::Right)?;
    /// assert_eq!(s.lrange(b"dst", 0, -1)?, [b"b".to_vec(), b"a".to_vec()]);
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    Right,
}

/// Where `LINSERT` places the new element relative to its pivot.
///
/// ```
/// use kevy_store::{InsertPosition, Store};
/// let mut s = Store::new();
/// s.rpush(b"l", &[b"a".as_slice(), b"c".as_slice()]).unwrap();
/// assert_eq!(s.linsert(b"l", InsertPosition::Before, b"c", b"b").unwrap(), 3);
/// assert_eq!(s.lrange(b"l", 0, -1).unwrap(), [b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InsertPosition {
    /// Immediately before the pivot.
    ///
    /// ```
    /// use kevy_store::{InsertPosition, Store};
    /// let mut s = Store::new();
    /// s.rpush(b"l", &[b"b".as_slice()])?;
    /// s.linsert(b"l", InsertPosition::Before, b"b", b"a")?;
    /// assert_eq!(s.lrange(b"l", 0, -1)?, [b"a".to_vec(), b"b".to_vec()]);
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    Before,
    /// Immediately after the pivot.
    ///
    /// ```
    /// use kevy_store::{InsertPosition, Store};
    /// let mut s = Store::new();
    /// s.rpush(b"l", &[b"a".as_slice()])?;
    /// s.linsert(b"l", InsertPosition::After, b"a", b"b")?;
    /// assert_eq!(s.lrange(b"l", 0, -1)?, [b"a".to_vec(), b"b".to_vec()]);
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    After,
}

/// Which score change a conditional `ZADD` accepts for an existing
/// member: `GT` / `LT`, or any. New members are not judged by it.
///
/// ```
/// use kevy_store::{ScoreCompare, SetCondition, ZaddFlags};
/// let gt = ZaddFlags::new(SetCondition::Always, ScoreCompare::Greater).unwrap();
/// assert_eq!(gt.compare(), ScoreCompare::Greater);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum ScoreCompare {
    /// Any new score replaces the old one.
    ///
    /// ```
    /// use kevy_store::{ScoreCompare, SetCondition, Store, ZaddFlags};
    /// let mut s = Store::new();
    /// let any = ZaddFlags::new(SetCondition::Always, ScoreCompare::Any).unwrap();
    /// s.zadd(b"z", &[(5.0, b"m".as_slice())])?;
    /// s.zadd_flags(b"z", &[(1.0, b"m".as_slice())], any)?;
    /// assert_eq!(s.zscore(b"z", b"m")?, Some(1.0));
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    #[default]
    Any,
    /// `GT`: only a greater score replaces the old one.
    ///
    /// ```
    /// use kevy_store::{ScoreCompare, SetCondition, Store, ZaddFlags};
    /// let mut s = Store::new();
    /// let gt = ZaddFlags::new(SetCondition::Always, ScoreCompare::Greater).unwrap();
    /// s.zadd(b"z", &[(5.0, b"m".as_slice())])?;
    /// s.zadd_flags(b"z", &[(1.0, b"m".as_slice())], gt)?;
    /// assert_eq!(s.zscore(b"z", b"m")?, Some(5.0));
    /// s.zadd_flags(b"z", &[(9.0, b"m".as_slice())], gt)?;
    /// assert_eq!(s.zscore(b"z", b"m")?, Some(9.0));
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    Greater,
    /// `LT`: only a lesser score replaces the old one.
    ///
    /// ```
    /// use kevy_store::{ScoreCompare, SetCondition, Store, ZaddFlags};
    /// let mut s = Store::new();
    /// let lt = ZaddFlags::new(SetCondition::Always, ScoreCompare::Less).unwrap();
    /// s.zadd(b"z", &[(5.0, b"m".as_slice())])?;
    /// s.zadd_flags(b"z", &[(9.0, b"m".as_slice())], lt)?;
    /// assert_eq!(s.zscore(b"z", b"m")?, Some(5.0));
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    Less,
}
