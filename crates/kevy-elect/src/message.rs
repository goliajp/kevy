//! Wire message types for `kevy-elect`'s control plane.
//!
//! All messages travel as RESP2 multi-bulk arrays (same format as
//! the kevy keyspace plane), so encode/decode reuses `kevy-resp`'s
//! borrowed parser. See [`docs/protocol.md`](../../docs/protocol.md)
//! for the wire shape per variant and the state machine that
//! consumes them.
//!
//! Verbs (UPPERCASE bulk strings on the wire) — uniform with kevy's
//! existing command shape:
//!
//! - `HB <epoch> <node_id> <role> <repl_offset>`
//! - `OFFER <new_epoch> <candidate_id> <repl_offset>`
//! - `ACCEPT <epoch> <accepter_id>`
//! - `ANNOUNCE <epoch> <new_primary_id> <new_primary_addr>`
//!
//! The numeric fields (epoch, offset) ride as RESP bulk-string
//! decimals — same convention as `kevy-replicate`'s
//! `REPLICATE FROM <offset> ID <replica_id>` handshake. Keeps every
//! frame text-friendly for tcpdump / strace debugging.
//!
//! ```
//! use kevy_elect::message::{Message, Role};
//!
//! let hb = Message::Hb { epoch: 3, node_id: "n1".into(), role: Role::Replica, repl_offset: 42 };
//! assert_eq!(hb.encode(), b"*5\r\n$2\r\nHB\r\n$1\r\n3\r\n$2\r\nn1\r\n$7\r\nreplica\r\n$2\r\n42\r\n");
//! ```

/// Self-perceived role of a node in its heartbeat. The state
/// machine in `kevy-elect`'s reactor decides which transitions are
/// legal; this enum is just what gets put on the wire.
///
/// ```
/// use kevy_elect::Role;
///
/// let role = Role::parse(b"replica");
/// assert_eq!(role, Some(Role::Replica));
/// assert_eq!(role.map(Role::as_str), Some("replica"));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Role {
    /// This node currently accepts writes.
    ///
    /// ```
    /// use std::time::Instant;
    /// use kevy_elect::{Elector, Message, Role};
    ///
    /// // the bootstrap node starts as primary and says so in every heartbeat
    /// let mut a = Elector::new("a", vec!["a".into(), "b".into()], "10.0.0.1:6004", Role::Primary);
    /// let out = a.tick(Instant::now());
    /// assert!(matches!(out[0].msg, Message::Hb { role: Role::Primary, .. }));
    /// ```
    Primary,
    /// This node mirrors a primary.
    ///
    /// ```
    /// use std::time::Instant;
    /// use kevy_elect::{Elector, Message, Role};
    ///
    /// // a primary that hears of a primary in a newer epoch steps down to replica
    /// let mut a = Elector::new("a", vec!["a".into(), "b".into()], "10.0.0.1:6004", Role::Primary);
    /// let hb = Message::Hb { epoch: 2, node_id: "b".into(), role: Role::Primary, repl_offset: 0 };
    /// a.on_message("b", hb, Instant::now());
    /// assert_eq!((a.role(), a.current_primary()), (Role::Replica, Some("b")));
    /// ```
    Replica,
    /// This node has sent `OFFER` for the current epoch and is
    /// waiting for quorum `ACCEPT`. Transitional — once enough
    /// ACCEPTs arrive it flips to `Primary` and broadcasts
    /// `ANNOUNCE`; if the election times out it flips back to
    /// `Replica` and re-arms its DOWN detector.
    ///
    /// ```
    /// use std::time::{Duration, Instant};
    /// use kevy_elect::{Elector, Message, Role};
    ///
    /// let mut b = Elector::new("b", vec!["a".into(), "b".into()], "10.0.0.2:6004", Role::Replica);
    /// let t0 = Instant::now();
    /// let hb = Message::Hb { epoch: 1, node_id: "a".into(), role: Role::Primary, repl_offset: 0 };
    /// b.on_message("a", hb, t0);
    /// // the primary stays silent past `down_after` (5 s): b offers itself
    /// b.tick(t0 + Duration::from_secs(5));
    /// assert_eq!((b.role(), b.epoch()), (Role::Candidate, 2));
    /// ```
    Candidate,
}

impl Role {
    /// The wire-form lowercase ASCII for this role.
    ///
    /// ```
    /// use kevy_elect::Role;
    ///
    /// assert_eq!(Role::Candidate.as_str(), "candidate");
    /// assert_eq!(Role::parse(Role::Primary.as_str().as_bytes()), Some(Role::Primary));
    /// ```
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Primary => "primary",
            Self::Replica => "replica",
            Self::Candidate => "candidate",
        }
    }

    /// Parse the wire-form (case-insensitive).
    ///
    /// ```
    /// use kevy_elect::Role;
    ///
    /// assert_eq!(Role::parse(b"PRIMARY"), Some(Role::Primary));
    /// assert_eq!(Role::parse(b"leader"), None);
    /// ```
    pub fn parse(s: &[u8]) -> Option<Self> {
        if s.eq_ignore_ascii_case(b"primary") {
            Some(Self::Primary)
        } else if s.eq_ignore_ascii_case(b"replica") {
            Some(Self::Replica)
        } else if s.eq_ignore_ascii_case(b"candidate") {
            Some(Self::Candidate)
        } else {
            None
        }
    }
}

/// One decoded message off the control wire. The four variants
/// mirror the four verbs in the protocol spec.
///
/// ```
/// use kevy_elect::Message;
///
/// let vote = Message::Accept { epoch: 5, accepter_id: "n3".into() };
/// let (decoded, used) = Message::decode(&vote.encode())?;
/// assert_eq!(decoded, vote);
/// assert_eq!(used, vote.encode().len());
/// # Ok::<(), kevy_elect::DecodeError>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Message {
    /// `HB <epoch> <node_id> <role> <repl_offset>` — heartbeat.
    /// Sent every `hb_interval_ms` (default 200 ms) by every node
    /// to every other peer. Receiver updates its per-peer
    /// last-seen + cached view; there is no ACK.
    ///
    /// ```
    /// use std::time::Instant;
    /// use kevy_elect::{Elector, Message, Role};
    ///
    /// let mut a = Elector::new("a", vec!["a".into(), "b".into(), "c".into()], "10.0.0.1:6004", Role::Primary);
    /// let out = a.tick(Instant::now());
    /// // one heartbeat to every peer but itself
    /// let to: Vec<&str> = out.iter().map(|o| o.to.as_str()).collect();
    /// assert_eq!(to, ["b", "c"]);
    /// assert!(out.iter().all(|o| matches!(o.msg, Message::Hb { .. })));
    /// ```
    Hb {
        /// Election epoch the sender believes is current.
        ///
        /// ```
        /// use std::time::Instant;
        /// use kevy_elect::{Elector, Message, Role};
        ///
        /// let mut b = Elector::new("b", vec!["a".into(), "b".into()], "10.0.0.2:6004", Role::Replica);
        /// let hb = Message::Hb { epoch: 4, node_id: "a".into(), role: Role::Primary, repl_offset: 0 };
        /// b.on_message("a", hb, Instant::now());
        /// assert_eq!(b.epoch(), 4); // a primary in a newer epoch is followed
        /// ```
        epoch: u64,
        /// Sender's node id (operator-declared, stable, unique).
        ///
        /// ```
        /// use std::time::Instant;
        /// use kevy_elect::{Elector, Message, Role};
        ///
        /// let mut a = Elector::new("a", vec!["a".into(), "b".into()], "10.0.0.1:6004", Role::Primary);
        /// let Message::Hb { node_id, .. } = &a.tick(Instant::now())[0].msg else { panic!("not a heartbeat") };
        /// assert_eq!(node_id, "a");
        /// ```
        node_id: String,
        /// Sender's self-perceived role.
        ///
        /// ```
        /// use std::time::Instant;
        /// use kevy_elect::{Elector, Message, Role};
        ///
        /// let mut b = Elector::new("b", vec!["a".into(), "b".into()], "10.0.0.2:6004", Role::Replica);
        /// let hb = Message::Hb { epoch: 1, node_id: "a".into(), role: Role::Primary, repl_offset: 0 };
        /// b.on_message("a", hb, Instant::now());
        /// assert_eq!(b.current_primary(), Some("a")); // learned from the role flag
        /// ```
        role: Role,
        /// Highest applied replication offset on the sender.
        ///
        /// ```
        /// use std::time::Instant;
        /// use kevy_elect::{Elector, Message, Role};
        ///
        /// let mut a = Elector::new("a", vec!["a".into(), "b".into()], "10.0.0.1:6004", Role::Replica);
        /// a.set_repl_offset(1024);
        /// let Message::Hb { repl_offset, .. } = a.tick(Instant::now())[0].msg else { panic!("not a heartbeat") };
        /// assert_eq!(repl_offset, 1024);
        /// ```
        repl_offset: u64,
    },

    /// `OFFER <new_epoch> <candidate_id> <repl_offset>` — a
    /// replica that flagged the primary DOWN AND won candidate-
    /// selection (highest offset → lowest node-id) broadcasts
    /// this to ask for quorum ACCEPT.
    ///
    /// ```
    /// use std::time::{Duration, Instant};
    /// use kevy_elect::{Elector, Message, Outbound, Role};
    ///
    /// let mut b = Elector::new("b", vec!["a".into(), "b".into()], "10.0.0.2:6004", Role::Replica);
    /// b.set_repl_offset(7);
    /// let t0 = Instant::now();
    /// let hb = Message::Hb { epoch: 1, node_id: "a".into(), role: Role::Primary, repl_offset: 7 };
    /// b.on_message("a", hb, t0);
    /// let out = b.tick(t0 + Duration::from_secs(5));
    /// let offer = Message::Offer { new_epoch: 2, candidate_id: "b".into(), repl_offset: 7 };
    /// assert!(out.iter().any(|o| o.to == Outbound::BROADCAST && o.msg == offer));
    /// ```
    Offer {
        /// Strictly greater than every previously-seen epoch.
        ///
        /// ```
        /// use std::time::Instant;
        /// use kevy_elect::{Elector, Message, Role};
        ///
        /// let mut c = Elector::new("c", vec!["b".into(), "c".into()], "10.0.0.3:6004", Role::Replica);
        /// let offer = |new_epoch| Message::Offer { new_epoch, candidate_id: "b".into(), repl_offset: 0 };
        /// assert!(c.on_message("b", offer(1), Instant::now()).is_empty()); // not newer than c's epoch 1
        /// assert_eq!(c.on_message("b", offer(2), Instant::now()).len(), 1);
        /// ```
        new_epoch: u64,
        /// Candidate's node id.
        ///
        /// ```
        /// use std::time::Instant;
        /// use kevy_elect::{Elector, Message, Role};
        ///
        /// let mut c = Elector::new("c", vec!["b".into(), "c".into()], "10.0.0.3:6004", Role::Replica);
        /// let offer = Message::Offer { new_epoch: 2, candidate_id: "b".into(), repl_offset: 0 };
        /// assert_eq!(c.on_message("b", offer, Instant::now())[0].to, "b"); // the vote goes back to it
        /// ```
        candidate_id: String,
        /// Candidate's `repl_offset` — peers reject the OFFER if
        /// they themselves have a higher offset (a better
        /// candidate must exist).
        ///
        /// ```
        /// use std::time::Instant;
        /// use kevy_elect::{Elector, Message, Role};
        ///
        /// let mut c = Elector::new("c", vec!["b".into(), "c".into()], "10.0.0.3:6004", Role::Replica);
        /// c.set_repl_offset(500);
        /// let behind = Message::Offer { new_epoch: 2, candidate_id: "b".into(), repl_offset: 400 };
        /// assert!(c.on_message("b", behind, Instant::now()).is_empty());
        /// ```
        repl_offset: u64,
    },

    /// `ACCEPT <epoch> <accepter_id>` — a peer's vote for an
    /// `OFFER`. Each peer casts at most ONE accept per epoch
    /// (prevents two candidates from gathering quorum in the same
    /// round).
    ///
    /// ```
    /// use std::time::Instant;
    /// use kevy_elect::{Elector, Message, Role};
    ///
    /// let mut c = Elector::new("c", vec!["a".into(), "b".into(), "c".into()], "10.0.0.3:6004", Role::Replica);
    /// let now = Instant::now();
    /// let from_a = Message::Offer { new_epoch: 2, candidate_id: "a".into(), repl_offset: 0 };
    /// let from_b = Message::Offer { new_epoch: 2, candidate_id: "b".into(), repl_offset: 0 };
    /// assert_eq!(c.on_message("a", from_a, now)[0].msg, Message::Accept { epoch: 2, accepter_id: "c".into() });
    /// assert!(c.on_message("b", from_b, now).is_empty()); // one vote per epoch
    /// ```
    Accept {
        /// The epoch being voted for.
        ///
        /// ```
        /// use std::time::Instant;
        /// use kevy_elect::{Elector, Message, Role};
        ///
        /// let mut c = Elector::new("c", vec!["b".into(), "c".into()], "10.0.0.3:6004", Role::Replica);
        /// let offer = Message::Offer { new_epoch: 9, candidate_id: "b".into(), repl_offset: 0 };
        /// let Message::Accept { epoch, .. } = c.on_message("b", offer, Instant::now())[0].msg else { panic!("no vote") };
        /// assert_eq!(epoch, 9);
        /// ```
        epoch: u64,
        /// The voter's node id.
        ///
        /// ```
        /// use std::time::Instant;
        /// use kevy_elect::{Elector, Message, Role};
        ///
        /// let mut c = Elector::new("c", vec!["b".into(), "c".into()], "10.0.0.3:6004", Role::Replica);
        /// let offer = Message::Offer { new_epoch: 2, candidate_id: "b".into(), repl_offset: 0 };
        /// let out = c.on_message("b", offer, Instant::now());
        /// let Message::Accept { accepter_id, .. } = &out[0].msg else { panic!("no vote") };
        /// assert_eq!(accepter_id, "c");
        /// ```
        accepter_id: String,
    },

    /// `ANNOUNCE <epoch> <new_primary_id> <new_primary_addr>` —
    /// the winning candidate broadcasts this on hitting quorum
    /// `N/2 + 1` ACCEPTs. Peers update their `current_epoch` and
    /// `current_primary`, then retarget `kevy-replicate` at the
    /// new primary. The old primary (if alive) sees this with a
    /// newer epoch and demotes.
    ///
    /// ```
    /// use std::time::Instant;
    /// use kevy_elect::{Elector, Message, Role};
    ///
    /// let mut a = Elector::new("a", vec!["a".into(), "b".into()], "10.0.0.1:6004", Role::Primary);
    /// let won = Message::Announce { epoch: 2, new_primary_id: "b".into(), new_primary_addr: "10.0.0.2:6004".into() };
    /// a.on_message("b", won, Instant::now());
    /// assert_eq!((a.role(), a.current_primary(), a.epoch()), (Role::Replica, Some("b"), 2));
    /// ```
    Announce {
        /// The new election epoch.
        ///
        /// ```
        /// use std::time::Instant;
        /// use kevy_elect::{Elector, Message, Role};
        ///
        /// let mut c = Elector::new("c", vec!["b".into(), "c".into()], "10.0.0.3:6004", Role::Replica);
        /// let won = Message::Announce { epoch: 6, new_primary_id: "b".into(), new_primary_addr: "b:6004".into() };
        /// c.on_message("b", won, Instant::now());
        /// assert_eq!(c.epoch(), 6);
        /// ```
        epoch: u64,
        /// New primary's node id.
        ///
        /// ```
        /// use std::time::Instant;
        /// use kevy_elect::{Elector, Message, Role};
        ///
        /// let mut c = Elector::new("c", vec!["b".into(), "c".into()], "10.0.0.3:6004", Role::Replica);
        /// // an announce naming this node itself makes it primary
        /// let won = Message::Announce { epoch: 2, new_primary_id: "c".into(), new_primary_addr: "c:6004".into() };
        /// c.on_message("b", won, Instant::now());
        /// assert_eq!((c.role(), c.current_primary()), (Role::Primary, Some("c")));
        /// ```
        new_primary_id: String,
        /// New primary's `host:port` (the kevy compat port, where
        /// the `REPLICAOF` handshake connects).
        ///
        /// ```
        /// use std::time::{Duration, Instant};
        /// use kevy_elect::{Elector, Message, Role};
        ///
        /// // a one-node cluster with no primary elects itself after the grace window
        /// let mut a = Elector::new("a", vec!["a".into()], "10.0.0.1:6004", Role::Replica);
        /// let t0 = Instant::now();
        /// a.tick(t0);
        /// let out = a.tick(t0 + Duration::from_secs(5));
        /// let addr = out.iter().find_map(|o| match &o.msg {
        ///     Message::Announce { new_primary_addr, .. } => Some(new_primary_addr.as_str()),
        ///     _ => None,
        /// });
        /// assert_eq!(addr, Some("10.0.0.1:6004")); // the address given to `Elector::new`
        /// ```
        new_primary_addr: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_round_trip() {
        for r in [Role::Primary, Role::Replica, Role::Candidate] {
            assert_eq!(Role::parse(r.as_str().as_bytes()), Some(r));
        }
    }

    #[test]
    fn role_parse_case_insensitive() {
        assert_eq!(Role::parse(b"PRIMARY"), Some(Role::Primary));
        assert_eq!(Role::parse(b"Replica"), Some(Role::Replica));
        assert_eq!(Role::parse(b"caNDidaTE"), Some(Role::Candidate));
    }

    #[test]
    fn role_parse_unknown_is_none() {
        assert_eq!(Role::parse(b"leader"), None);
        assert_eq!(Role::parse(b""), None);
    }
}
