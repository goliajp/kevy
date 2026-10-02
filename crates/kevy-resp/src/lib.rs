//! kevy-resp — a zero-dependency [RESP] (REdis Serialization Protocol) codec.
//!
//! It covers what a client sends to drive commands — the RESP2 multi-bulk
//! request (`*N\r\n$len\r\n…`) and the inline form (a bare `PING\r\n` typed over
//! a raw connection) — plus the reply primitives a server writes back. Parsing
//! is incremental and allocation-light: [`parse_command`] returns `Ok(None)`
//! when more bytes are needed, so it composes with a streaming read loop.
//!
//! Pure Rust, no dependencies. Part of the [kevy] key–value server.
//!
//! [RESP]: https://redis.io/docs/latest/develop/reference/protocol-spec/
//! [kevy]: https://crates.io/crates/kevy
//!
//! # Example
//!
//! ```
//! use kevy_resp::{encode_bulk, encode_simple_string, parse_command};
//!
//! // Parse one command from a request buffer.
//! let (cmd, consumed) = parse_command(b"*2\r\n$4\r\nECHO\r\n$2\r\nhi\r\n")
//!     .unwrap() // not a protocol error
//!     .unwrap(); // a complete frame was present
//! assert_eq!(cmd, vec![b"ECHO".to_vec(), b"hi".to_vec()]);
//! assert_eq!(consumed, 22);
//!
//! // A partial frame asks for more bytes rather than erroring.
//! assert_eq!(parse_command(b"*1\r\n$4\r\nPI").unwrap(), None);
//!
//! // Encode replies into a caller-owned buffer.
//! let mut out = Vec::new();
//! encode_simple_string(&mut out, "PONG");
//! encode_bulk(&mut out, b"hi");
//! assert_eq!(out, b"+PONG\r\n$2\r\nhi\r\n");
//! ```
#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod argv;
mod argv_borrowed;
mod argv_pool;
mod argv_view;
mod dtoa;
mod error;
pub mod fuzz;
mod inline_ranges;
pub mod ops_table;
mod pubsub_event;
mod reply;
mod reply_encode;
mod reply_encode_resp3;
mod reply_parse;
mod request;
mod request_borrowed;
mod resp3_nulls;
pub mod verb_arity;

pub use argv::{Argv, Command};
pub use argv_borrowed::ArgvBorrowed;
pub use argv_pool::ArgvPool;
pub use argv_view::{ArgvIter, ArgvView};
pub use dtoa::write_double;
pub use error::{CmdError, ProtocolError};
pub use pubsub_event::PubsubEvent;
pub use reply::Reply;
pub use reply_encode::{
    encode_array_len, encode_bulk, encode_command, encode_command_borrowed, encode_error,
    encode_integer, encode_null_bulk, encode_simple_string,
};
pub use reply_encode_resp3::{
    encode_big_number, encode_blob_error, encode_boolean, encode_double, encode_map_header,
    encode_null, encode_push_header, encode_set_header, encode_verbatim,
};
pub use reply_parse::{TextedReply, parse_reply, parse_reply_keeping_double_text};
pub use request::{MAX_BULK_LEN, MAX_MULTIBULK_LEN, parse_command, parse_command_into};
pub use request_borrowed::parse_command_borrowed;
pub use resp3_nulls::resp3_nulls;

/// Which version of RESP a connection is speaking. Negotiated via the
/// `HELLO` command — RESP2 is the default for backwards compatibility
/// with every Redis 6.x and earlier client; RESP3 is opt-in via
/// `HELLO 3` and unlocks the additive reply types
/// ([`Reply::Map`] / [`Reply::Set`] / [`Reply::Double`] / [`Reply::Boolean`]
/// / [`Reply::Verbatim`] / [`Reply::BigNumber`] / [`Reply::Null`] /
/// [`Reply::Push`] / [`Reply::BlobError`]) plus out-of-band push frames
/// for `PUBLISH` delivery.
///
/// Stored per-connection in `kevy-rt` and forwarded to dispatch so each
/// reply encoder can pick the right wire shape — see the kevy v2 RESP3
/// design notes for the full phase plan.
///
/// ```
/// use kevy_resp::RespVersion;
///
/// // a connection starts on RESP2 until the client sends `HELLO 3`
/// let mut version = RespVersion::default();
/// assert_eq!(version, RespVersion::V2);
/// version = RespVersion::V3;
/// assert_ne!(version, RespVersion::V2);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RespVersion {
    /// RESP2 — every reply is one of the seven legacy prefixes
    /// (`+ - : $ * $-1 *-1`). Default for backward compatibility.
    ///
    /// ```
    /// use kevy_resp::RespVersion;
    /// assert_eq!(RespVersion::default(), RespVersion::V2);
    /// ```
    #[default]
    V2,
    /// RESP3 — adds 9 reply prefixes (`% ~ , # = ( _ > !`) plus
    /// attributes (`|`). Opt-in via `HELLO 3`.
    ///
    /// ```
    /// use kevy_resp::{RespVersion, encode_boolean};
    ///
    /// // RESP3-only reply types are written once the connection is on V3
    /// let version = RespVersion::V3;
    /// let mut out = Vec::new();
    /// if version == RespVersion::V3 {
    ///     encode_boolean(&mut out, true);
    /// }
    /// assert_eq!(out, b"#t\r\n");
    /// ```
    V3,
}

const _: () = {
    const fn send_sync<T: Send + Sync>() {}
    send_sync::<Argv>();
    send_sync::<ArgvBorrowed<'static>>();
    send_sync::<ArgvPool>();
    send_sync::<ArgvIter<'static, Argv>>();
    send_sync::<CmdError>();
    send_sync::<ProtocolError>();
    send_sync::<Reply>();
    send_sync::<PubsubEvent>();
    send_sync::<RespVersion>();
    send_sync::<ops_table::OpSpec>();
    send_sync::<ops_table::NotifyKind>();
    send_sync::<fuzz::Lcg>();
    send_sync::<fuzz::Summary>();
    send_sync::<fuzz::FuzzResult>();
};
