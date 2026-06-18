//! Inbound-message handlers + policy helpers for [`crate::Elector`]
//! — pulled out of `elector.rs` so that file stays under the
//! project's 500-LOC ceiling. Same `impl Elector`, called from
//! [`crate::Elector::on_message`] and the tick path.

use std::time::Instant;

use crate::Elector;
use crate::elector::{Outbound, PeerView};
use crate::message::{Message, Role};

impl Elector {
    pub(crate) fn on_hb(
        &mut self,
        from: &str,
        epoch: u64,
        role: Role,
        repl_offset: u64,
        now: Instant,
    ) {
        // Stamp the per-peer view.
        self.peer_views.insert(
            from.to_string(),
            PeerView {
                last_seen: now,
                last_epoch: epoch,
                last_role: role,
                last_repl_offset: repl_offset,
            },
        );
        // A HB from a sender claiming `Primary` at epoch ≥ self
        // tells us who currently writes. Three cases:
        // (a) self has no primary yet AND epoch matches → just
        //     learn the primary (bootstrap path — operators don't
        //     send a boot ANNOUNCE; the in-effect primary
        //     advertises itself via its HB's role flag).
        // (b) epoch > self.epoch → demote / retarget without
        //     waiting for ANNOUNCE (we may have missed it during a
        //     partition).
        // (c) otherwise → no state change.
        if role == Role::Primary
            && epoch >= self.epoch
            && (epoch > self.epoch
                || (self.current_primary.is_none() && self.role == Role::Replica))
        {
            self.epoch = epoch;
            self.current_primary = Some(from.to_string());
            if self.role == Role::Primary && from != self.node_id {
                self.role = Role::Replica;
            } else if self.role == Role::Candidate {
                // Lost the election we were running.
                self.role = Role::Replica;
                self.offer_at = None;
                self.accept_votes.clear();
            }
        }
    }

    pub(crate) fn on_offer(
        &mut self,
        new_epoch: u64,
        candidate_id: String,
        repl_offset: u64,
        out: &mut Vec<Outbound>,
    ) {
        // Stale-epoch OFFER: silently reject.
        if new_epoch <= self.epoch {
            return;
        }
        // I have a higher offset → reject (a better candidate
        // exists — me or someone else).
        if self.my_repl_offset > repl_offset {
            return;
        }
        // Tie on offset → lower node-id wins. I refuse if my id is
        // strictly less than the candidate's.
        if self.my_repl_offset == repl_offset && self.node_id.as_bytes() < candidate_id.as_bytes() {
            return;
        }
        // Already voted in this epoch.
        if self.last_accept_epoch == Some(new_epoch) {
            return;
        }
        // Accept.
        self.last_accept_epoch = Some(new_epoch);
        self.epoch = new_epoch;
        out.push(Outbound {
            to: candidate_id,
            msg: Message::Accept {
                epoch: new_epoch,
                accepter_id: self.node_id.clone(),
            },
        });
    }

    pub(crate) fn on_accept(&mut self, epoch: u64, accepter_id: String, now: Instant, out: &mut Vec<Outbound>) {
        if self.role != Role::Candidate || self.epoch != epoch {
            return;
        }
        self.accept_votes.insert(accepter_id, ());
        // Don't broadcast ANNOUNCE here — `tick` checks the tally
        // and emits ANNOUNCE on the next call. (Lets a single test
        // tick capture all-in-one: trigger ACCEPTs by calling
        // on_message N times, then call tick once.)
        let _ = now;
        let _ = out;
    }

    pub(crate) fn on_announce(
        &mut self,
        epoch: u64,
        new_primary_id: String,
        _new_primary_addr: String,
        _out: &mut Vec<Outbound>,
    ) {
        // Stale ANNOUNCE.
        if epoch <= self.epoch && self.current_primary.as_deref() == Some(new_primary_id.as_str()) {
            return;
        }
        if epoch < self.epoch {
            return;
        }
        // Commit.
        self.epoch = epoch;
        self.current_primary = Some(new_primary_id.clone());
        self.last_accept_epoch = Some(epoch);
        self.accept_votes.clear();
        self.offer_at = None;
        if new_primary_id == self.node_id {
            self.role = Role::Primary;
        } else {
            // Old-primary rejoin / sibling-replica acknowledgement:
            // either way, I'm now a replica.
            self.role = Role::Replica;
        }
    }

    // ─────────── policy helpers ───────────

    pub(crate) fn quorum_size(&self) -> usize {
        self.peer_ids.len() / 2 + 1
    }

    pub(crate) fn is_peer_down(&self, peer: &str, now: Instant) -> bool {
        match self.peer_views.get(peer) {
            Some(v) => now.duration_since(v.last_seen) >= self.config.down_after,
            None => true,
        }
    }

    #[cfg(test)]
    pub(crate) fn force_known_primary(&mut self, primary: &str) {
        self.current_primary = Some(primary.to_string());
    }

    pub(crate) fn am_best_candidate(&self, now: Instant) -> bool {
        // Among ALIVE peers (excluding self + the dead primary),
        // I must have the highest offset; ties broken by lowest
        // node-id.
        for other in &self.peer_ids {
            if other == &self.node_id {
                continue;
            }
            if self.current_primary.as_deref() == Some(other) {
                continue;
            }
            let Some(v) = self.peer_views.get(other) else {
                // Never heard from this peer — assume alive but
                // unknown offset. Conservatively skip (we don't
                // race against unseen peers).
                continue;
            };
            if now.duration_since(v.last_seen) >= self.config.down_after {
                continue; // peer down — not in tie-break
            }
            if v.last_repl_offset > self.my_repl_offset {
                return false;
            }
            if v.last_repl_offset == self.my_repl_offset
                && other.as_bytes() < self.node_id.as_bytes()
            {
                return false;
            }
        }
        true
    }
}
