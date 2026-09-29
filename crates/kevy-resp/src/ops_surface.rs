//! Surface bits: where an op is implemented **today**. Absence of a
//! bit is ground truth, not aspiration — should-exist-but-doesn't
//! lives in [`KNOWN_GAPS`](super::KNOWN_GAPS), which parity tests keep exhaustive.
//!
//! ```
//! use kevy_resp::ops_table::{spec, surface};
//!
//! let set = spec("SET").expect("SET is registered");
//! assert_ne!(set.surfaces & surface::SERVER, 0);
//! assert_ne!(set.surfaces & surface::REPLAY, 0);
//! ```

/// Server RESP dispatch (`kevy` crate).
///
/// ```
/// use kevy_resp::ops_table::{spec, surface};
/// assert_ne!(spec("GET").unwrap().surfaces & surface::SERVER, 0);
/// ```
pub const SERVER: u16 = 1 << 0;
/// Embedded `Store` facade method (`kevy-embedded`).
///
/// ```
/// use kevy_resp::ops_table::{ops_with, surface};
/// assert!(ops_with(surface::ESTORE).contains(&"HSET"));
/// ```
pub const ESTORE: u16 = 1 << 1;
/// Embedded `Pipeline` entry.
///
/// ```
/// use kevy_resp::ops_table::{spec, surface};
/// assert_ne!(spec("INCR").unwrap().surfaces & surface::PIPE, 0);
/// assert_eq!(spec("APPEND").unwrap().surfaces & surface::PIPE, 0);
/// ```
pub const PIPE: u16 = 1 << 2;
/// Embedded `AtomicCtx` **and** `AtomicAllShards` (the two must
/// never drift — the parity test asserts both).
///
/// ```
/// use kevy_resp::ops_table::{spec, surface};
/// assert_ne!(spec("LPUSH").unwrap().surfaces & surface::ATOMIC, 0);
/// ```
pub const ATOMIC: u16 = 1 << 3;
/// Embedded AOF replay arm (`replay.rs`) — REQUIRED for every
/// verb any embedded facade logs, and for every server verb an
/// embed-as-replica must apply.
///
/// ```
/// use kevy_resp::ops_table::{spec, surface};
/// // every embedded write needs a replay arm, or reopening loses it
/// let del = spec("DEL").unwrap();
/// assert!(del.write && del.surfaces & surface::REPLAY != 0);
/// ```
pub const REPLAY: u16 = 1 << 4;
/// AOF rewrite emit set (`kevy-persist::rewrite_fmt`).
///
/// ```
/// use kevy_resp::ops_table::{spec, surface};
/// assert_ne!(spec("ZADD").unwrap().surfaces & surface::REWRITE, 0);
/// assert_eq!(spec("INCR").unwrap().surfaces & surface::REWRITE, 0);
/// ```
pub const REWRITE: u16 = 1 << 5;
