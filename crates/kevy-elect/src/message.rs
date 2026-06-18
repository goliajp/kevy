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

/// Self-perceived role of a node in its heartbeat. The state
/// machine in `kevy-elect`'s reactor decides which transitions are
/// legal; this enum is just what gets put on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// This node currently accepts writes.
    Primary,
    /// This node mirrors a primary.
    Replica,
    /// This node has sent `OFFER` for the current epoch and is
    /// waiting for quorum `ACCEPT`. Transitional — once enough
    /// ACCEPTs arrive it flips to `Primary` and broadcasts
    /// `ANNOUNCE`; if the election times out it flips back to
    /// `Replica` and re-arms its DOWN detector.
    Candidate,
}

impl Role {
    /// The wire-form lowercase ASCII for this role.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Primary => "primary",
            Self::Replica => "replica",
            Self::Candidate => "candidate",
        }
    }

    /// Parse the wire-form (case-insensitive).
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
#[derive(Debug, Clone)]
pub enum Message {
    /// `HB <epoch> <node_id> <role> <repl_offset>` — heartbeat.
    /// Sent every `hb_interval_ms` (default 200 ms) by every node
    /// to every other peer. Receiver updates its per-peer
    /// last-seen + cached view; there is no ACK.
    Hb {
        /// Election epoch the sender believes is current.
        epoch: u64,
        /// Sender's node id (operator-declared, stable, unique).
        node_id: String,
        /// Sender's self-perceived role.
        role: Role,
        /// Highest applied replication offset on the sender.
        repl_offset: u64,
    },

    /// `OFFER <new_epoch> <candidate_id> <repl_offset>` — a
    /// replica that flagged the primary DOWN AND won candidate-
    /// selection (highest offset → lowest node-id) broadcasts
    /// this to ask for quorum ACCEPT.
    Offer {
        /// Strictly greater than every previously-seen epoch.
        new_epoch: u64,
        /// Candidate's node id.
        candidate_id: String,
        /// Candidate's `repl_offset` — peers reject the OFFER if
        /// they themselves have a higher offset (a better
        /// candidate must exist).
        repl_offset: u64,
    },

    /// `ACCEPT <epoch> <accepter_id>` — a peer's vote for an
    /// `OFFER`. Each peer casts at most ONE accept per epoch
    /// (prevents two candidates from gathering quorum in the same
    /// round).
    Accept {
        /// The epoch being voted for.
        epoch: u64,
        /// The voter's node id.
        accepter_id: String,
    },

    /// `ANNOUNCE <epoch> <new_primary_id> <new_primary_addr>` —
    /// the winning candidate broadcasts this on hitting quorum
    /// `N/2 + 1` ACCEPTs. Peers update their `current_epoch` and
    /// `current_primary`, then retarget `kevy-replicate` at the
    /// new primary. The old primary (if alive) sees this with a
    /// newer epoch and demotes.
    Announce {
        /// The new election epoch.
        epoch: u64,
        /// New primary's node id.
        new_primary_id: String,
        /// New primary's `host:port` (the kevy compat port, where
        /// the v1.18 `REPLICAOF` handshake connects).
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
