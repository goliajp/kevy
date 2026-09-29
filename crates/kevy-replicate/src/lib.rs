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
//! A replica against a primary built from this crate's primary-side
//! pieces — the backlog ([`source`]), the handshake parser and `+ACK`
//! ([`handshake`]) — applying each frame to a map:
//!
//! ```
//! use std::collections::HashMap;
//! use std::io::{Read, Write};
//! use kevy_replicate::feed::FeedPosition;
//! use kevy_replicate::handshake::{HandshakeReq, encode_ack};
//! use kevy_replicate::replica::ReplicaClient;
//! use kevy_replicate::source::ReplicationSource;
//!
//! // primary: two applied writes sit in the backlog
//! let mut backlog = ReplicationSource::new(1 << 20);
//! for cmd in [["SET", "a", "1"], ["SET", "b", "2"]] {
//!     backlog.push_mutation(&kevy_resp::Argv::from(cmd.map(|w| w.as_bytes().to_vec()).to_vec()));
//! }
//! let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
//! let addr = listener.local_addr()?;
//! let primary = std::thread::spawn(move || -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
//!     let (mut sock, _) = listener.accept()?;
//!     let (mut buf, mut argv) = (Vec::new(), kevy_resp::Argv::default());
//!     while kevy_resp::parse_command_into(&buf, &mut argv)?.is_none() {
//!         let mut chunk = [0u8; 256];
//!         let n = sock.read(&mut chunk)?;
//!         buf.extend_from_slice(&chunk[..n]);
//!     }
//!     let req = HandshakeReq::parse(&argv)?;
//!     sock.write_all(&encode_ack(FeedPosition::new(1, req.from.offset)))?;
//!     for frame in backlog.frames_from(req.from.offset).map_err(|e| format!("{e:?}"))? {
//!         sock.write_all(&frame.bytes)?;
//!     }
//!     Ok(()) // closing the socket ends the stream
//! });
//!
//! // replica: apply every frame in offset order
//! let mut store = HashMap::new();
//! for frame in ReplicaClient::connect(addr, "replica-a", 0)? {
//!     let frame = frame?;
//!     if let [b"SET", key, value] = frame.argv.iter().collect::<Vec<_>>()[..] {
//!         store.insert(key.to_vec(), value.to_vec());
//!     }
//! }
//! primary.join().expect("primary thread").map_err(|e| e.to_string())?;
//! assert_eq!(store.get(&b"b"[..]).map(Vec::as_slice), Some(&b"2"[..]));
//! assert_eq!(store.len(), 2);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! In kevy itself the dispatcher is `kevy::KevyCommands::dispatch`
//! over a `kevy::KeyspaceStore`: the frames carry the exact argv the
//! primary applied, so the replica's store ends up byte-equivalent.
//!
//! # Features
//!
//! - `secure` (default) — Noise IK encrypted, mutually authenticated
//!   links: `replica::ReplicaSecurity` and
//!   `replica::ConnectOptions::with_security`. Off, a replica connects
//!   in plaintext only and the handshake code is not linked.
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
mod replica_connect;
mod replica_decode;
mod replica_error;
mod replica_event;
#[cfg(not(feature = "secure"))]
#[path = "replica_plain.rs"]
mod replica_secure;
#[cfg(feature = "secure")]
mod replica_secure;
pub mod slot;
pub mod source;
pub mod wire;
mod wire_snapshot;

const _: () = {
    const fn send_sync<T: Send + Sync>() {}
    send_sync::<feed::FeedPosition>();
    send_sync::<feed::FeedRead>();
    send_sync::<feed::FeedFrame<'static>>();
    send_sync::<feed::FeedSource>();
    send_sync::<handshake::HandshakeReq>();
    send_sync::<handshake::HandshakeError>();
    send_sync::<replica::DecodedFrame>();
    send_sync::<replica::ReplicaEvent>();
    send_sync::<replica::ReplicaClient>();
    send_sync::<replica::ReplicaError>();
    #[cfg(feature = "secure")]
    send_sync::<replica::ReplicaSecurity>();
    send_sync::<replica::ConnectOptions>();
    send_sync::<slot::ReplicaSlot>();
    send_sync::<slot::SlotTable>();
    send_sync::<source::Frame>();
    send_sync::<source::FromOffset>();
    send_sync::<source::ReplicationSource>();
    send_sync::<source::FramesIter<'static>>();
    send_sync::<wire::WireError>();
    send_sync::<wire::SnapshotMarker>();
};
