use std::time::{Duration, Instant};

use crate::elector::{ElectConfig, ElectJitter, Elector};
use crate::message::{Message, Role};
use crate::persist::ElectorPersist;

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    /// One persisted `(epoch, voted_for)` pair.
    type SavedPair = (u64, Option<String>);

    /// In-memory [`ElectorPersist`] that records every `save` call in
    /// order (the audit trail for the write-discipline tests) and
    /// serves the latest pair back through `load` — so "rebuild the
    /// Elector with the same persist" models a process restart.
    #[derive(Clone, Default)]
    struct MemPersist {
        state: Arc<Mutex<SavedPair>>,
        log: Arc<Mutex<Vec<SavedPair>>>,
    }

    impl MemPersist {
        fn log(&self) -> Vec<SavedPair> {
            self.log.lock().unwrap().clone()
        }
    }

    impl ElectorPersist for MemPersist {
        fn save(&self, epoch: u64, voted_for: Option<&str>) {
            let pair = (epoch, voted_for.map(str::to_string));
            *self.state.lock().unwrap() = pair.clone();
            self.log.lock().unwrap().push(pair);
        }

        fn load(&self) -> (u64, Option<String>) {
            self.state.lock().unwrap().clone()
        }
    }

    fn now() -> Instant {
        Instant::now()
    }

    fn cfg_fast() -> ElectConfig {
        ElectConfig {
            hb_interval: Duration::from_millis(10),
            down_after: Duration::from_millis(100),
            election_timeout: Duration::from_millis(80),
            election_backoff: Duration::from_millis(20),
            election_backoff_jitter: Duration::from_millis(0),
        }
    }

    fn mk(node: &str, peers: &[&str], role: Role) -> Elector {
        Elector::new(
            node,
            peers.iter().map(|s| s.to_string()).collect(),
            format!("10.0.0.{}:6004", node.bytes().last().unwrap_or(b'1') as u32 - 48),
            role,
            cfg_fast(),
            ElectJitter::Fixed(Duration::from_millis(0)),
        )
    }

    #[test]
    fn quorum_3_is_2() {
        let e = mk("a", &["a", "b", "c"], Role::Replica);
        assert_eq!(e.quorum_size(), 2);
    }

    #[test]
    fn quorum_5_is_3() {
        let e = mk("a", &["a", "b", "c", "d", "e"], Role::Replica);
        assert_eq!(e.quorum_size(), 3);
    }

    #[test]
    fn quorum_2_is_2_degenerate() {
        let e = mk("a", &["a", "b"], Role::Replica);
        assert_eq!(e.quorum_size(), 2);
    }

    #[test]
    fn tick_emits_one_hb_per_peer_per_interval() {
        let mut e = mk("a", &["a", "b", "c"], Role::Replica);
        let t0 = now();
        let out = e.tick(t0);
        // 2 peers (b, c), self skipped → 2 HBs.
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|o| matches!(o.msg, Message::Hb { .. })));
        // A second tick within `hb_interval` emits nothing.
        let out2 = e.tick(t0 + Duration::from_millis(1));
        assert!(out2.is_empty());
        // After hb_interval elapses, another batch.
        let out3 = e.tick(t0 + Duration::from_millis(11));
        assert_eq!(out3.len(), 2);
    }

    #[test]
    fn replica_with_alive_primary_does_not_elect() {
        let mut e = mk("a", &["a", "b", "c"], Role::Replica);
        e.force_known_primary("c"); // c is primary
        let t0 = now();
        // Primary alive — HB just received.
        e.on_message(
            "c",
            Message::Hb {
                epoch: 1,
                node_id: "c".to_string(),
                role: Role::Primary,
                repl_offset: 100,
            },
            t0,
        );
        let out = e.tick(t0);
        assert!(out.iter().all(|o| matches!(o.msg, Message::Hb { .. })));
        assert_eq!(e.role(), Role::Replica);
    }

    #[test]
    fn replica_starts_candidacy_when_primary_down_and_im_best() {
        let mut e = mk("a", &["a", "b", "c"], Role::Replica);
        e.force_known_primary("c"); // c is primary
        e.set_repl_offset(100);
        let t0 = now();
        // No HB from primary ever → DOWN by default. Also b is
        // alive but with lower offset.
        e.on_message(
            "b",
            Message::Hb {
                epoch: 1,
                node_id: "b".to_string(),
                role: Role::Replica,
                repl_offset: 50,
            },
            t0,
        );
        let out = e.tick(t0 + Duration::from_millis(101)); // DOWN window crossed
        // Expect: some HBs + one OFFER broadcast.
        let offers: Vec<_> = out
            .iter()
            .filter(|o| matches!(o.msg, Message::Offer { .. }))
            .collect();
        assert_eq!(offers.len(), 1, "expected one OFFER, out: {out:?}");
        assert_eq!(e.role(), Role::Candidate);
        assert_eq!(e.epoch(), 2);
    }

    #[test]
    fn candidate_with_lower_offset_loses_to_higher() {
        let mut e = mk("a", &["a", "b", "c"], Role::Replica);
        e.set_repl_offset(50);
        // Primary c never sends HB → DOWN-by-default at any tick
        // time. b sends a recent HB so it's still alive when the
        // tick fires — the test point is: b's higher offset
        // disqualifies a from candidacy.
        let t0 = now();
        let trigger_at = t0 + Duration::from_millis(101); // > down_after for primary
        e.on_message(
            "b",
            Message::Hb {
                epoch: 1,
                node_id: "b".to_string(),
                role: Role::Replica,
                repl_offset: 100,
            },
            trigger_at - Duration::from_millis(20), // 20 ms < 100 ms down_after
        );
        e.force_known_primary("c");
        let out = e.tick(trigger_at);
        // a should NOT have started candidacy (b is better).
        assert!(
            !out.iter().any(|o| matches!(o.msg, Message::Offer { .. })),
            "out: {out:?}"
        );
        assert_eq!(e.role(), Role::Replica);
    }

    #[test]
    fn quorum_accept_promotes_candidate_to_primary() {
        let mut e = mk("a", &["a", "b", "c"], Role::Replica);
        e.set_repl_offset(100);
        e.force_known_primary("c");
        let t0 = now();
        e.on_message(
            "b",
            Message::Hb {
                epoch: 1,
                node_id: "b".to_string(),
                role: Role::Replica,
                repl_offset: 50,
            },
            t0,
        );
        // Start candidacy.
        let _ = e.tick(t0 + Duration::from_millis(101));
        assert_eq!(e.role(), Role::Candidate);
        // Peer b ACCEPTs.
        e.on_message(
            "b",
            Message::Accept {
                epoch: 2,
                accepter_id: "b".to_string(),
            },
            t0 + Duration::from_millis(102),
        );
        // Self-vote (1) + b's vote (1) = 2 = quorum for N=3.
        let out = e.tick(t0 + Duration::from_millis(103));
        let announces: Vec<_> = out
            .iter()
            .filter(|o| matches!(o.msg, Message::Announce { .. }))
            .collect();
        assert_eq!(announces.len(), 1);
        assert_eq!(e.role(), Role::Primary);
        assert_eq!(e.current_primary(), Some("a"));
    }

    #[test]
    fn replica_demotes_on_announce_for_newer_epoch() {
        let mut e = mk("a", &["a", "b", "c"], Role::Primary);
        let t0 = now();
        e.epoch = 1;
        // Sibling claims higher epoch.
        e.on_message(
            "b",
            Message::Announce {
                epoch: 2,
                new_primary_id: "b".to_string(),
                new_primary_addr: "10.0.0.2:6004".to_string(),
            },
            t0,
        );
        assert_eq!(e.role(), Role::Replica);
        assert_eq!(e.current_primary(), Some("b"));
        assert_eq!(e.epoch(), 2);
    }

    #[test]
    fn announce_for_self_keeps_primary_role() {
        let mut e = mk("a", &["a", "b", "c"], Role::Candidate);
        e.epoch = 5;
        let t0 = now();
        e.on_message(
            "a",
            Message::Announce {
                epoch: 5,
                new_primary_id: "a".to_string(),
                new_primary_addr: "10.0.0.1:6004".to_string(),
            },
            t0,
        );
        assert_eq!(e.role(), Role::Primary);
    }

    #[test]
    fn dueling_candidates_tied_offset_lower_node_id_wins() {
        // a + b tie on offset; a's id < b's. b receives a's OFFER
        // with the same offset, rejects → no ACCEPT from b.
        // Symmetrically, a receives b's OFFER with same offset
        // and HIGHER node-id → a ACCEPTs.
        let mut a = mk("a", &["a", "b", "c"], Role::Replica);
        let mut b = mk("b", &["a", "b", "c"], Role::Replica);
        a.set_repl_offset(100);
        b.set_repl_offset(100);

        let out_b_to_a = b.on_message(
            "a", // received from "a"
            Message::Offer {
                new_epoch: 2,
                candidate_id: "a".to_string(),
                repl_offset: 100,
            },
            now(),
        );
        assert!(out_b_to_a.iter().any(|o| matches!(o.msg, Message::Accept { .. })),
            "b should ACCEPT a's OFFER (a < b by node-id tiebreak)");

        let out_a_to_b = a.on_message(
            "b",
            Message::Offer {
                new_epoch: 2,
                candidate_id: "b".to_string(),
                repl_offset: 100,
            },
            now(),
        );
        assert!(out_a_to_b.is_empty(),
            "a should REJECT b's OFFER (a < b)");
    }

    #[test]
    fn accept_only_once_per_epoch() {
        let mut e = mk("c", &["a", "b", "c"], Role::Replica);
        let out1 = e.on_message(
            "a",
            Message::Offer {
                new_epoch: 2,
                candidate_id: "a".to_string(),
                repl_offset: 100,
            },
            now(),
        );
        assert_eq!(out1.len(), 1);
        // b ALSO sends an OFFER in the same epoch — c must NOT
        // ACCEPT again.
        let out2 = e.on_message(
            "b",
            Message::Offer {
                new_epoch: 2,
                candidate_id: "b".to_string(),
                repl_offset: 100,
            },
            now(),
        );
        assert!(out2.is_empty(), "c already voted in epoch 2");
    }

    #[test]
    fn stale_epoch_offer_rejected() {
        let mut e = mk("c", &["a", "b", "c"], Role::Replica);
        e.epoch = 5;
        let out = e.on_message(
            "a",
            Message::Offer {
                new_epoch: 3, // stale
                candidate_id: "a".to_string(),
                repl_offset: 999,
            },
            now(),
        );
        assert!(out.is_empty());
    }

    #[test]
    fn old_primary_rejoin_demotes_on_higher_epoch_hb() {
        let mut e = mk("a", &["a", "b", "c"], Role::Primary);
        e.epoch = 1;
        let t0 = now();
        // Partition healed — sibling reports epoch 2 as primary.
        e.on_message(
            "b",
            Message::Hb {
                epoch: 2,
                node_id: "b".to_string(),
                role: Role::Primary,
                repl_offset: 200,
            },
            t0,
        );
        assert_eq!(e.role(), Role::Replica);
        assert_eq!(e.current_primary(), Some("b"));
        assert_eq!(e.epoch(), 2);
    }

    #[test]
    fn election_timeout_falls_back_to_replica_with_backoff() {
        let mut e = mk("a", &["a", "b", "c"], Role::Replica);
        e.set_repl_offset(100);
        e.force_known_primary("c");
        let t0 = now();
        e.on_message(
            "b",
            Message::Hb {
                epoch: 1,
                node_id: "b".to_string(),
                role: Role::Replica,
                repl_offset: 50,
            },
            t0,
        );
        // Start candidacy.
        let _ = e.tick(t0 + Duration::from_millis(101));
        assert_eq!(e.role(), Role::Candidate);
        // No ACCEPTs arrive. Election timeout fires.
        let _ = e.tick(t0 + Duration::from_millis(101 + 81));
        assert_eq!(e.role(), Role::Replica);
        assert!(e.backoff_until.is_some());
    }

    #[test]
    fn split_brain_minority_cannot_promote() {
        // N=3, minority = 1 node (a). a sees primary c DOWN +
        // partner b DOWN. a starts candidacy, but no ACCEPTs
        // arrive → timeout → backoff. role stays Replica
        // (never reaches Primary).
        let mut a = mk("a", &["a", "b", "c"], Role::Replica);
        a.set_repl_offset(100);
        a.force_known_primary("c");
        let t0 = now();
        // a starts candidacy (no peer HBs since boot → DOWN-by-default).
        let _ = a.tick(t0 + Duration::from_millis(101));
        assert_eq!(a.role(), Role::Candidate);
        // No ACCEPTs (b + c are partitioned away).
        let _ = a.tick(t0 + Duration::from_millis(101 + 81));
        // Falls back to Replica.
        assert_eq!(a.role(), Role::Replica);
        // a NEVER reached Primary — split-brain protected.
        assert!(a.current_primary() != Some("a"));
    }

    #[test]
    fn n2_degenerate_needs_both_alive() {
        // N=2, quorum=2. a starts candidacy with self-vote = 1,
        // needs b's ACCEPT to reach 2. Without b, falls back.
        let mut a = mk("a", &["a", "b"], Role::Replica);
        a.set_repl_offset(100);
        a.force_known_primary("b");
        let t0 = now();
        let _ = a.tick(t0 + Duration::from_millis(101));
        assert_eq!(a.role(), Role::Candidate);
        // Election times out, no ACCEPT.
        let _ = a.tick(t0 + Duration::from_millis(101 + 81));
        assert_eq!(a.role(), Role::Replica);
    }

    // ─────────── epoch/votedFor persistence ───────────

    #[test]
    fn restart_with_persisted_vote_rejects_same_epoch_offer() {
        // c votes for A in epoch 2, "crashes", and is rebuilt from
        // the same persist. B's OFFER in the SAME epoch must be
        // rejected — the Raft one-vote-per-epoch guarantee has to
        // survive the restart.
        let p = MemPersist::default();
        let mut c = mk("c", &["a", "b", "c"], Role::Replica)
            .with_persist(Box::new(p.clone()));
        let out = c.on_message(
            "a",
            Message::Offer {
                new_epoch: 2,
                candidate_id: "a".to_string(),
                repl_offset: 100,
            },
            now(),
        );
        assert!(
            out.iter().any(|o| matches!(o.msg, Message::Accept { .. })),
            "c should ACCEPT a's OFFER pre-restart"
        );
        // "Restart": fresh Elector, same persist backend.
        let mut c2 = mk("c", &["a", "b", "c"], Role::Replica)
            .with_persist(Box::new(p.clone()));
        assert_eq!(c2.epoch(), 2, "epoch must be restored from persist");
        let out2 = c2.on_message(
            "b",
            Message::Offer {
                new_epoch: 2, // same epoch, different candidate
                candidate_id: "b".to_string(),
                repl_offset: 999,
            },
            now(),
        );
        assert!(
            out2.is_empty(),
            "restarted c must NOT vote again in epoch 2: {out2:?}"
        );
    }

    #[test]
    fn restart_recovers_epoch_and_elects_with_fresh_epoch() {
        // a runs a candidacy at epoch 2, "crashes", rebuilds from
        // the same persist: the restored elector must not re-issue
        // an OFFER under an already-consumed epoch — its next
        // candidacy runs at epoch 3.
        let p = MemPersist::default();
        let mut a = mk("a", &["a", "b", "c"], Role::Replica)
            .with_persist(Box::new(p.clone()));
        a.set_repl_offset(100);
        a.force_known_primary("c");
        let t0 = now();
        let _ = a.tick(t0 + Duration::from_millis(101));
        assert_eq!(a.role(), Role::Candidate);
        assert_eq!(a.epoch(), 2);
        // "Restart".
        let mut a2 = mk("a", &["a", "b", "c"], Role::Replica)
            .with_persist(Box::new(p.clone()));
        assert_eq!(a2.epoch(), 2, "restored epoch, not the boot default 1");
        a2.set_repl_offset(100);
        a2.force_known_primary("c");
        let out = a2.tick(t0 + Duration::from_millis(300));
        let offer_epoch = out.iter().find_map(|o| match &o.msg {
            Message::Offer { new_epoch, .. } => Some(*new_epoch),
            _ => None,
        });
        assert_eq!(
            offer_epoch,
            Some(3),
            "post-restart candidacy must bump PAST the persisted epoch"
        );
    }

    #[test]
    fn save_is_called_before_accept_leaves_the_elector() {
        // Write-discipline audit: `save` runs synchronously inside
        // `on_offer` BEFORE the ACCEPT is pushed to the outbound
        // batch — so by the time any ACCEPT is observable, the vote
        // is already durable. The MemPersist call log must therefore
        // show exactly one save, carrying the voted epoch+candidate,
        // the moment the ACCEPT surfaces.
        let p = MemPersist::default();
        let mut c = mk("c", &["a", "b", "c"], Role::Replica)
            .with_persist(Box::new(p.clone()));
        assert!(p.log().is_empty(), "no save before any vote");
        let out = c.on_message(
            "a",
            Message::Offer {
                new_epoch: 2,
                candidate_id: "a".to_string(),
                repl_offset: 100,
            },
            now(),
        );
        assert!(out.iter().any(|o| matches!(o.msg, Message::Accept { .. })));
        assert_eq!(
            p.log(),
            vec![(2, Some("a".to_string()))],
            "the vote was persisted (epoch 2, votedFor=a) before the ACCEPT surfaced"
        );
        // Rejected offers must NOT touch the persist (b's same-epoch
        // OFFER is refused → no new save entry).
        let out2 = c.on_message(
            "b",
            Message::Offer {
                new_epoch: 2,
                candidate_id: "b".to_string(),
                repl_offset: 100,
            },
            now(),
        );
        assert!(out2.is_empty());
        assert_eq!(p.log().len(), 1, "a refused OFFER persists nothing");
    }

    #[test]
    fn candidacy_bump_is_persisted_with_self_vote() {
        // Starting a candidacy = bumping the epoch + implicitly
        // voting for self; both must be durable before the OFFER
        // broadcast surfaces.
        let p = MemPersist::default();
        let mut a = mk("a", &["a", "b", "c"], Role::Replica)
            .with_persist(Box::new(p.clone()));
        a.set_repl_offset(100);
        a.force_known_primary("c");
        let out = a.tick(now() + Duration::from_millis(101));
        assert!(out.iter().any(|o| matches!(o.msg, Message::Offer { .. })));
        assert_eq!(p.log(), vec![(2, Some("a".to_string()))]);
    }

    #[test]
    fn higher_epoch_follow_is_persisted() {
        // Following a higher epoch (primary HB / ANNOUNCE) is an
        // epoch move and must be durable too — a restarted node
        // must not run an election under an epoch older than one
        // it already witnessed.
        let p = MemPersist::default();
        let mut a = mk("a", &["a", "b", "c"], Role::Replica)
            .with_persist(Box::new(p.clone()));
        a.on_message(
            "b",
            Message::Hb {
                epoch: 7,
                node_id: "b".to_string(),
                role: Role::Primary,
                repl_offset: 10,
            },
            now(),
        );
        assert_eq!(a.epoch(), 7);
        assert_eq!(p.log(), vec![(7, None)], "epoch follow persisted, no vote");
        // Restart: epoch restored, no vote restored.
        let a2 = mk("a", &["a", "b", "c"], Role::Replica)
            .with_persist(Box::new(p.clone()));
        assert_eq!(a2.epoch(), 7);
    }
}
