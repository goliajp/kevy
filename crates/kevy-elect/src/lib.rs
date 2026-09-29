//! kevy-elect — quorum-based primary failover for kevy.
//!
//! A layer on top of the manual
//! `REPLICAOF` primitive. Detects a primary's death by quorum
//! heartbeat, runs an offset-ordered election among the live
//! replicas, promotes the winner via `REPLICAOF NO ONE`, and
//! retargets the survivors at the new primary. Driven by an
//! operator-declared peer list (no gossip discovery — the peer set
//! is static for the lifetime of a cluster generation).
//!
//! The protocol spec lives in `docs/protocol.md`; message
//! types in [`mod@message`]. The heartbeat loop, DOWN detector, and
//! election machinery build on top of those.
//!
//! A three-node failover, driven by hand:
//!
//! ```
//! use std::time::{Duration, Instant};
//! use kevy_elect::{Elector, Message, Role};
//!
//! let peers = || vec!["a".to_string(), "b".to_string(), "c".to_string()];
//! let mut b = Elector::new("b", peers(), "b:6004", Role::Replica);
//! let mut c = Elector::new("c", peers(), "c:6004", Role::Replica);
//! b.set_repl_offset(100);
//! c.set_repl_offset(50);
//!
//! let t0 = Instant::now();
//! let hb_a = Message::Hb { epoch: 1, node_id: "a".into(), role: Role::Primary, repl_offset: 100 };
//! b.on_message("a", hb_a.clone(), t0);
//! c.on_message("a", hb_a, t0);
//!
//! // a falls silent; after `down_after` b, holding the most data, offers itself
//! let t1 = t0 + Duration::from_secs(5);
//! let offer = Message::Offer { new_epoch: 2, candidate_id: "b".into(), repl_offset: 100 };
//! assert!(b.tick(t1).iter().any(|o| o.msg == offer));
//! let vote = c.on_message("b", offer, t1);
//! b.on_message("c", vote[0].msg.clone(), t1);
//!
//! // two of three votes is a quorum: b announces and takes over
//! let announce = b.tick(t1);
//! assert_eq!((b.role(), b.epoch()), (Role::Primary, 2));
//! let msg = announce.iter().find(|o| matches!(o.msg, Message::Announce { .. })).map(|o| o.msg.clone());
//! if let Some(m) = msg {
//!     c.on_message("b", m, t1);
//! }
//! assert_eq!(c.current_primary(), Some("b"));
//! ```
#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod config;
pub mod elector;
mod elector_inbound;
mod elector_tick;
mod link;
pub mod message;
pub mod persist;
#[cfg(test)]
pub mod sim;
pub mod transport;
mod transport_loops;
pub mod wire;

pub use link::SecureLinks;
pub use transport::{ElectorSnapshot, PeerAddr, TopologyCallback, Transport};

#[cfg(test)]
#[path = "elector_tests.rs"]
mod elector_tests;

pub use elector::{ElectConfig, ElectJitter, Elector, Outbound};
pub use message::{Message, Role};
pub use persist::{ElectorPersist, NoPersist};
pub use wire::DecodeError;

const _: () = {
    const fn send_sync<T: Send + Sync>() {}
    const fn send<T: Send>() {}
    send_sync::<ElectConfig>();
    send_sync::<ElectJitter>();
    send_sync::<Outbound>();
    send_sync::<Message>();
    send_sync::<Role>();
    send_sync::<DecodeError>();
    send_sync::<SecureLinks>();
    send_sync::<PeerAddr>();
    send_sync::<ElectorSnapshot>();
    send_sync::<Transport>();
    send_sync::<NoPersist>();
    // the persistence backend is `dyn ElectorPersist + Send`, so the
    // elector moves between threads but is shared only behind a lock
    send::<Elector>();
};
