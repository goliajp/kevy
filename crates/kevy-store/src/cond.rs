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
    #[default]
    Always,
    /// `NX`: write only when the target does not exist.
    IfAbsent,
    /// `XX`: write only when the target already exists.
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
    Left,
    /// The tail: `RPUSH` / `RPOP` side.
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
    Before,
    /// Immediately after the pivot.
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
    #[default]
    Any,
    /// `GT`: only a greater score replaces the old one.
    Greater,
    /// `LT`: only a lesser score replaces the old one.
    Less,
}
