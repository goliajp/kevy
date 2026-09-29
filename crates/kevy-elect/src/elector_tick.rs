//! The tick path of [`crate::Elector`] — heartbeat scheduling, DOWN
//! detection and the candidacy lifecycle — kept apart from `elector.rs`
//! so that file stays under the project's 500-LOC ceiling. Same
//! `impl Elector`, called from [`crate::Elector::tick`].

use std::time::Instant;

use crate::Elector;
use crate::elector::Outbound;
use crate::message::{Message, Role};

impl Elector {
    pub(crate) fn emit_heartbeats(&mut self, now: Instant, out: &mut Vec<Outbound>) {
        // One HB per peer per `hb_interval`. Per-peer schedule
        // staggers (a peer added later gets its own clock).
        for peer in self.peer_ids.clone() {
            if peer == self.node_id {
                continue;
            }
            let due = match self.last_hb_sent.get(&peer) {
                Some(prev) => now.duration_since(*prev) >= self.config.hb_interval,
                None => true,
            };
            if due {
                self.last_hb_sent.insert(peer.clone(), now);
                out.push(Outbound {
                    to: peer,
                    msg: Message::Hb {
                        epoch: self.epoch,
                        node_id: self.node_id.clone(),
                        role: self.role,
                        repl_offset: self.my_repl_offset,
                    },
                });
            }
        }
    }

    /// Preconditions for starting a candidacy: replica role, out of
    /// backoff, primary DOWN by my view, and this node winning the
    /// candidate-selection ordering.
    fn election_preconditions_met(&self, now: Instant) -> bool {
        // Only replicas start elections.
        if self.role != Role::Replica {
            return false;
        }
        // In backoff after a failed candidacy.
        if let Some(b) = self.backoff_until
            && now < b
        {
            return false;
        }
        // Primary must be DOWN by my view. A cluster with NO known
        // primary (cold start where every node defers to the
        // election — the role clamp makes this the normal
        // boot) counts as down after one `down_after` grace window,
        // giving a live primary's HB time to reach us first.
        if let Some(primary) = self.current_primary.clone() {
            if !self.is_peer_down(&primary, now) {
                return false;
            }
        } else {
            let seen_enough =
                self.first_tick.is_some_and(|t| now.duration_since(t) >= self.config.down_after);
            if !seen_enough {
                return false;
            }
        }
        // Candidate-selection: I must have the highest offset AND
        // lowest node-id among alive peers (the primary is dead +
        // not in the tie-break set).
        self.am_best_candidate(now)
    }

    pub(crate) fn maybe_start_election(&mut self, now: Instant, out: &mut Vec<Outbound>) {
        if !self.election_preconditions_met(now) {
            return;
        }
        // Start the candidacy. Raft persistence rule: the bumped
        // epoch + the implicit self-vote must be durable BEFORE the
        // OFFER can leave this node — a crash right after the
        // broadcast must not restart into an elector that reuses
        // this epoch (or votes for someone else in it).
        let new_epoch = self.epoch.saturating_add(1);
        self.persist.save(new_epoch, Some(self.node_id.as_str()));
        self.epoch = new_epoch;
        self.role = Role::Candidate;
        self.accept_votes.clear();
        // Implicit self-vote — record ourselves in the tally so
        // single-peer-needed (N=1, degenerate) and quorum=2/N=2
        // both work.
        self.accept_votes.insert(self.node_id.clone());
        self.offer_at = Some(now);
        out.push(Outbound {
            to: Outbound::BROADCAST.to_string(),
            msg: Message::Offer {
                new_epoch: self.epoch,
                candidate_id: self.node_id.clone(),
                repl_offset: self.my_repl_offset,
            },
        });
    }

    pub(crate) fn maybe_finish_candidacy(&mut self, now: Instant, out: &mut Vec<Outbound>) {
        if self.role != Role::Candidate {
            return;
        }
        let Some(offer_at) = self.offer_at else {
            return;
        };
        let quorum = self.quorum_size();
        if self.accept_votes.len() >= quorum {
            // Won — broadcast ANNOUNCE and become primary.
            self.role = Role::Primary;
            self.current_primary = Some(self.node_id.clone());
            self.offer_at = None;
            self.accept_votes.clear();
            out.push(Outbound {
                to: Outbound::BROADCAST.to_string(),
                msg: Message::Announce {
                    epoch: self.epoch,
                    new_primary_id: self.node_id.clone(),
                    new_primary_addr: self.my_advertised_addr.clone(),
                },
            });
            return;
        }
        if now.duration_since(offer_at) >= self.config.election_timeout {
            // Lost / timed out — back off with jitter, fall back to
            // Replica.
            self.role = Role::Replica;
            self.offer_at = None;
            self.accept_votes.clear();
            let jitter =
                self.jitter.sample(self.config.election_backoff_jitter, now, &self.node_id);
            self.backoff_until = Some(now + self.config.election_backoff + jitter);
        }
    }
}
