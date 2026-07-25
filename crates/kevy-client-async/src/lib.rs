//! kevy-client-async — async client for kevy, runtime-agnostic core
//! with feature-gated transports for `tokio`, `smol`, and `async-std`.
//!
//! # Status
//!
//! Surface is intentionally a near-1:1 mirror of the blocking
//! [`kevy_client::Connection`], plus pipeline-first sugar (batching is
//! where async actually pays off). See `docs/async.md` for the full
//! guide.
//!
//! # Runtime selection
//!
//! Exactly one of the following Cargo features must be enabled:
//!
//! | feature     | transport                      |
//! |-------------|--------------------------------|
//! | `tokio`     | `tokio::net::TcpStream`        |
//! | `smol`      | `smol::net::TcpStream`         |
//! | `async-std` | `async_std::net::TcpStream`    |
//!
//! `default = ["tokio"]` is a dev convenience so `cargo test
//! --workspace` builds without flags; **lib consumers should set
//! `default-features = false`** and pick their runtime explicitly so
//! the wrong one is never silently inherited. Enabling zero or more
//! than one triggers a `compile_error!` from this crate.
//!
//! # Dep-rule exemption
//!
//! This crate is the sole carved exemption from the project's
//! 0-crates.io-dep rule. Rationale: the Rust async ecosystem has no
//! std-only viable substrate. The exemption is per-crate and per-dep:
//! `kevy-client-async` may dep tokio / smol / async-std (and only those
//! three) with `default-features = false` + minimum-surface features
//! and an inline `# EXEMPTION` Cargo.toml comment.
//!
//! # Error compatibility
//!
//! Every async method returns `std::io::Result<T>` with the same
//! `ErrorKind` variants the blocking [`kevy_client`](https://docs.rs/kevy-client)
//! surface produces. This is contract, not coincidence — it lets
//! caller code carry over without changing match arms.
//!
//! | source                                | `ErrorKind`        |
//! |---------------------------------------|--------------------|
//! | RESP `-ERR …` reply                   | `Other`            |
//! | unexpected reply variant              | `Other`            |
//! | malformed RESP frame                  | `InvalidData`      |
//! | server closed connection mid-read     | `UnexpectedEof`    |
//! | unknown URL scheme / bad port / etc.  | `InvalidInput`     |
//! | TLS / AUTH / embed URL scheme         | `Unsupported`      |
//! | underlying socket I/O                 | (native kind)      |
//!
//! Wider error context (the RESP error string, the unexpected
//! variant name) is carried in the `io::Error`'s message — fetch with
//! `.to_string()` / `.into_inner()`.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod cluster;
mod cluster_topology;
pub mod cmd_hash;
pub mod cmd_list;
pub mod cmd_set;
pub mod cmd_string;
pub mod cmd_zset;
pub mod codec;
pub mod conn;
pub mod pipeline;
pub mod pubsub;
mod reply;
pub mod subscriber;
pub mod transport;
pub mod url;

#[cfg(feature = "tokio")]
pub mod rt_tokio;

#[cfg(feature = "smol")]
pub mod rt_smol;

#[cfg(feature = "async-std")]
pub mod rt_async_std;

pub use codec::AsyncRespCodec;
pub use conn::AsyncConnection;
pub use transport::{AsyncRead, AsyncTransport, AsyncWrite, read, write_all};

// Compile-time runtime selection gate. We enforce
// **exactly-one**: zero enabled = no IO substrate (silent shell);
// more than one enabled = ambiguous transport pick + bloats the build
// with unused runtimes. Either fails the build with a clear message.

#[cfg(not(any(feature = "tokio", feature = "smol", feature = "async-std")))]
compile_error!(
    "kevy-client-async requires exactly one runtime feature to be enabled: \
    `tokio`, `smol`, or `async-std`. See the crate-level docs."
);

#[cfg(any(
    all(feature = "tokio", feature = "smol"),
    all(feature = "tokio", feature = "async-std"),
    all(feature = "smol", feature = "async-std"),
))]
compile_error!(
    "kevy-client-async: multiple runtime features enabled. Pick exactly \
    one of `tokio`, `smol`, or `async-std` — never two."
);
