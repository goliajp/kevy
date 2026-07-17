//! kevy-replicate — primary-to-replica streaming replication.
//!
//! One primary streams every applied
//! mutation to N read replicas over a long-lived TCP connection, using a
//! RESP3-extended frame format with an offset envelope. New replicas join
//! via an inline snapshot ship, then catch up from the live frame stream.
//!
//! - [`wire`] — RESP-based frame format (see `docs/wire.md`).
//! - `wire_snapshot` (internal) — snapshot-ship framing for the joining
//!   replica.
//! - [`source`] — primary-side bounded backlog indexed by offset.
//! - [`handshake`] — `REPLICATE FROM <offset> ID <id>` parse + `+ACK` format.
//! - [`slot`] — per-replica state + reconnect-window expiry.
//! - [`replica`] — replica-side blocking TCP client (handshake +
//!   frame-decoding iterator).
//!
//! # Applying replicated frames
//!
//! `ReplicaClient` yields decoded `(offset, Argv)` tuples; *applying*
//! them to a local store is the caller's responsibility — the right
//! dispatcher depends on where the replica's data lives. The wire
//! format intentionally carries the exact RESP argv the primary
//! applied, so any dispatcher that hands `Argv` through Redis-verb
//! routing produces a byte-equivalent local store.
//!
//! The canonical in-process recipe — drop into a fresh
//! `kevy::KeyspaceStore` and dispatch through `kevy::KevyCommands`:
//!
//! ```ignore
//! use kevy_replicate::replica::ReplicaClient;
//! let mut client = ReplicaClient::connect(("primary:16004"), "replica-a", 0)?;
//! let kevy = kevy::KevyCommands::new();
//! let mut store = kevy::KeyspaceStore::new();
//! for result in &mut client {
//!     let frame = result?;
//!     kevy.dispatch(&mut store, &frame.argv);
//! }
//! # Ok::<_, kevy_replicate::replica::ReplicaError>(())
//! ```
//!
//! See the `replica_apply_dispatch_mirrors_primary_store` integration
//! test in `crates/kevy/tests/replication.rs` for the pattern under
//! the full primary+replica end-to-end harness.
//!
//! The kevy binary also ships full **server-as-replica** mode (it
//! auto-spawns a `ReplicaClient` when `[replication] role = "replica"`,
//! routing frames into the reactor with re-replication suppression);
//! the in-process recipe above is for any user that wants to drive
//! replication themselves.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod feed;
pub mod handshake;
pub mod replica;
mod replica_error;
mod replica_decode;
pub mod slot;
pub mod source;
pub mod wire;
mod wire_snapshot;
