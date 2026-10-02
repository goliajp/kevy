//! [`NotifyKind`], the keyspace-notification class column of the
//! command table.

/// Keyspace-notification class of a command (the Redis class letter
/// each variant names).
///
/// ```
/// use kevy_resp::ops_table::{NotifyKind, spec};
/// assert_eq!(spec("LPUSH").and_then(|s| s.notify), Some(NotifyKind::List));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum NotifyKind {
    /// Redis notification class `$` (string commands).
    ///
    /// ```
    /// use kevy_resp::ops_table::{NotifyKind, spec};
    /// assert_eq!(spec("APPEND").unwrap().notify, Some(NotifyKind::String));
    /// ```
    String,
    /// Class `h`.
    ///
    /// ```
    /// use kevy_resp::ops_table::{NotifyKind, spec};
    /// assert_eq!(spec("HSET").unwrap().notify, Some(NotifyKind::Hash));
    /// ```
    Hash,
    /// Class `l`.
    ///
    /// ```
    /// use kevy_resp::ops_table::{NotifyKind, spec};
    /// assert_eq!(spec("RPUSH").unwrap().notify, Some(NotifyKind::List));
    /// ```
    List,
    /// Class `s`.
    ///
    /// ```
    /// use kevy_resp::ops_table::{NotifyKind, spec};
    /// assert_eq!(spec("SADD").unwrap().notify, Some(NotifyKind::Set));
    /// ```
    Set,
    /// Class `z`.
    ///
    /// ```
    /// use kevy_resp::ops_table::{NotifyKind, spec};
    /// assert_eq!(spec("ZADD").unwrap().notify, Some(NotifyKind::Zset));
    /// ```
    Zset,
    /// Class `t`.
    ///
    /// ```
    /// use kevy_resp::ops_table::{NotifyKind, spec};
    /// assert_eq!(spec("XADD").unwrap().notify, Some(NotifyKind::Stream));
    /// ```
    Stream,
    /// Class `g` (DEL / EXPIRE / PERSIST …).
    ///
    /// ```
    /// use kevy_resp::ops_table::{NotifyKind, spec};
    /// assert_eq!(spec("DEL").unwrap().notify, Some(NotifyKind::Generic));
    /// ```
    Generic,
}
