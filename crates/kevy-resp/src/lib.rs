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
mod error;
mod reply_encode;
mod reply_parse;
mod request;

pub use argv::{Argv, Command};
pub use error::ProtocolError;
pub use reply_encode::{
    encode_array_len, encode_bulk, encode_command, encode_error, encode_integer, encode_null_bulk,
    encode_simple_string,
};
pub use reply_parse::{Reply, parse_reply};
pub use request::{parse_command, parse_command_into};
