//! Single command bodies, for a caller that reaches one directly rather
//! than through [`crate::exec`]: the server's `GET`/`SET` fast path and
//! its RESP3 reply shapes.
//!
//! ```
//! let mut store = kevy_store::Store::new();
//! let argv = kevy_resp::Argv::from(vec![b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()]);
//! let mut out = Vec::new();
//! kevy_verbs::cmd::set(&mut store, &argv, &mut out);
//! assert_eq!(out, b"+OK\r\n");
//! ```

pub use crate::mpop::{blmpop, bzmpop, lmpop, zmpop};
pub use crate::multiread::{lcs, zcombine};
#[cfg(feature = "streams-geo")]
pub use crate::stream::xinfo;
#[cfg(feature = "streams-geo")]
pub use crate::stream::xreadgroup_refusal;
pub use crate::stream_resp3::stream_resp3;
pub use crate::strings::set;
pub use crate::zset::parse_zadd_flags;
pub use crate::zset_pick::{
    bzpopmax, bzpopmin, zmscore, zpopmax, zpopmin, zrandmember, zrank, zrevrank,
};
pub use crate::zset_range::{zrange, zrangebyscore};
