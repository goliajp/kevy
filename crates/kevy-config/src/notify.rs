//! [`NotificationFlags`]: the parsed form of `notify_keyspace_events`, as a
//! set of bits the per-write check reads with one mask.

use crate::error::ValueError;

/// Parsed view of
/// [`NotificationSection::notify_keyspace_events`](crate::NotificationSection):
/// which channels publish and which event classes fire, one bit per Redis
/// flag letter. The runtime caches it per shard, so the per-write check is
/// a mask test.
///
/// Parse it from the flag string with [`str::parse`]; the `A` alias sets
/// [`NotificationFlags::ALL_CLASSES`], and any letter outside the Redis set
/// is refused rather than dropped.
///
/// ```
/// use kevy_config::NotificationFlags;
///
/// let f: NotificationFlags = "KEA".parse()?;
/// assert!(f.contains(NotificationFlags::KEYSPACE | NotificationFlags::EXPIRED));
/// assert!(!f.contains(NotificationFlags::NEW_KEY), "A leaves out n");
/// assert!(f.is_active());
/// assert!("KZ".parse::<NotificationFlags>().is_err());
/// # Ok::<(), kevy_config::ValueError>(())
/// ```
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct NotificationFlags(u16);

/// Flag letters in bit order, for parsing and display.
const LETTERS: [(char, NotificationFlags); 12] = [
    ('K', NotificationFlags::KEYSPACE),
    ('E', NotificationFlags::KEYEVENT),
    ('g', NotificationFlags::GENERIC),
    ('$', NotificationFlags::STRING),
    ('l', NotificationFlags::LIST),
    ('s', NotificationFlags::SET),
    ('h', NotificationFlags::HASH),
    ('z', NotificationFlags::ZSET),
    ('t', NotificationFlags::STREAM),
    ('x', NotificationFlags::EXPIRED),
    ('e', NotificationFlags::EVICTED),
    ('n', NotificationFlags::NEW_KEY),
];

impl NotificationFlags {
    /// No channel and no event class: notifications off.
    pub const NONE: Self = Self(0);
    /// `K` — publish on `__keyspace@<db>__:<key>`.
    pub const KEYSPACE: Self = Self(1 << 0);
    /// `E` — publish on `__keyevent@<db>__:<event>`.
    pub const KEYEVENT: Self = Self(1 << 1);
    /// `g` — DEL / EXPIRE / PERSIST / RENAME / TYPE / FLUSH etc.
    pub const GENERIC: Self = Self(1 << 2);
    /// `$` — SET / GETSET / INCR* / APPEND / MSET / etc.
    pub const STRING: Self = Self(1 << 3);
    /// `l` — LPUSH / RPUSH / LPOP / RPOP / LREM / LSET / LTRIM / …
    pub const LIST: Self = Self(1 << 4);
    /// `s` — SADD / SREM / SPOP / SMOVE / …
    pub const SET: Self = Self(1 << 5);
    /// `h` — HSET / HDEL / HINCRBY / HSETNX / …
    pub const HASH: Self = Self(1 << 6);
    /// `z` — ZADD / ZINCRBY / ZREM / ZREMRANGEBY* / …
    pub const ZSET: Self = Self(1 << 7);
    /// `t` — XADD / XDEL / XTRIM / XGROUP / XACK / XCLAIM / XREADGROUP …
    pub const STREAM: Self = Self(1 << 8);
    /// `x` — `expired` events, fired when a TTL'd key is removed (lazily
    /// on access or by the active reaper).
    pub const EXPIRED: Self = Self(1 << 9);
    /// `e` — `evicted` events, fired when maxmemory pressure removes a key.
    pub const EVICTED: Self = Self(1 << 10);
    /// `n` — `new` events, fired when a key is added to the keyspace. Not
    /// part of [`NotificationFlags::ALL_CLASSES`] (Redis convention).
    pub const NEW_KEY: Self = Self(1 << 11);
    /// `A` — every event class except `n`: the Redis alias for `g$lshztxe`.
    pub const ALL_CLASSES: Self = Self(
        Self::GENERIC.0
            | Self::STRING.0
            | Self::LIST.0
            | Self::SET.0
            | Self::HASH.0
            | Self::ZSET.0
            | Self::STREAM.0
            | Self::EXPIRED.0
            | Self::EVICTED.0,
    );
    const CHANNELS: u16 = Self::KEYSPACE.0 | Self::KEYEVENT.0;
    const CLASSES: u16 = Self::ALL_CLASSES.0 | Self::NEW_KEY.0;

    /// Whether every flag in `other` is set here.
    ///
    /// ```
    /// use kevy_config::NotificationFlags as F;
    /// let f = F::KEYSPACE | F::LIST;
    /// assert!(f.contains(F::LIST));
    /// assert!(!f.contains(F::LIST | F::SET));
    /// ```
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Set every flag in `other`.
    ///
    /// ```
    /// use kevy_config::NotificationFlags as F;
    /// let mut f = F::NONE;
    /// f.insert(F::KEYEVENT);
    /// assert!(f.contains(F::KEYEVENT));
    /// ```
    pub fn insert(&mut self, other: Self) {
        self.0 |= other.0;
    }

    /// Clear every flag in `other`.
    ///
    /// ```
    /// use kevy_config::NotificationFlags as F;
    /// let mut f = F::KEYSPACE | F::HASH;
    /// f.remove(F::HASH);
    /// assert_eq!(f, F::KEYSPACE);
    /// ```
    pub fn remove(&mut self, other: Self) {
        self.0 &= !other.0;
    }

    /// No flag set at all.
    ///
    /// ```
    /// use kevy_config::NotificationFlags as F;
    /// assert!(F::NONE.is_empty());
    /// assert!(!F::KEYSPACE.is_empty());
    /// ```
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Something can be published: at least one channel (`K`/`E`) and at
    /// least one event class. The hot path skips every notification
    /// before any classification or formatting when this is false.
    ///
    /// ```
    /// use kevy_config::NotificationFlags as F;
    /// assert!((F::KEYSPACE | F::STRING).is_active());
    /// assert!(!F::KEYSPACE.is_active(), "a channel with no class publishes nothing");
    /// assert!(!F::ALL_CLASSES.is_active(), "classes with no channel publish nothing");
    /// ```
    #[must_use]
    pub const fn is_active(self) -> bool {
        self.0 & Self::CHANNELS != 0 && self.0 & Self::CLASSES != 0
    }
}

impl core::ops::BitOr for NotificationFlags {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl core::ops::BitOrAssign for NotificationFlags {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

impl core::str::FromStr for NotificationFlags {
    type Err = ValueError;

    /// Parse a Redis-style flag string. An unknown letter is an error
    /// naming it — a typo'd flag string must fail config admission, not
    /// silently drop events.
    fn from_str(s: &str) -> Result<Self, ValueError> {
        let mut f = Self::NONE;
        for c in s.chars() {
            match LETTERS.iter().find(|(l, _)| *l == c) {
                Some((_, bit)) => f.insert(*bit),
                None if c == 'A' => f.insert(Self::ALL_CLASSES),
                None => return Err(ValueError::new(format!("unknown flag char {c:?}"))),
            }
        }
        Ok(f)
    }
}

/// The flag letters set, in canonical order (`A` is written out).
///
/// ```
/// let f: kevy_config::NotificationFlags = "xK".parse()?;
/// assert_eq!(f.to_string(), "Kx");
/// # Ok::<(), kevy_config::ValueError>(())
/// ```
impl core::fmt::Display for NotificationFlags {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        for (l, bit) in LETTERS {
            if self.contains(bit) {
                core::fmt::Write::write_char(f, l)?;
            }
        }
        Ok(())
    }
}

/// Shows the flag letters, so an empty set still prints.
///
/// ```
/// assert_eq!(format!("{:?}", kevy_config::NotificationFlags::NONE), "NotificationFlags(\"\")");
/// ```
impl core::fmt::Debug for NotificationFlags {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_tuple("NotificationFlags").field(&self.to_string()).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::NotificationFlags as F;

    #[test]
    fn every_letter_round_trips_and_a_is_everything_but_n() {
        for c in "KEg$lshztxen".chars() {
            let f: F = c.to_string().parse().expect("a known letter");
            assert_eq!(f.to_string(), c.to_string());
        }
        let a: F = "A".parse().expect("the alias");
        assert_eq!(a.to_string(), "g$lshztxe");
        assert_eq!(a, F::ALL_CLASSES);
        let all: F = "KEA n".replace(' ', "").parse().expect("all letters");
        assert!(all.contains(F::NEW_KEY) && all.is_active());
    }

    #[test]
    fn unknown_letters_are_refused_by_name() {
        let e = "KEZ".parse::<F>().expect_err("Z is not a flag");
        assert_eq!(e.to_string(), "unknown flag char 'Z'");
        assert_eq!("".parse::<F>().ok(), Some(F::NONE));
    }
}
