//! Unit tests for [`super`] — the replication state instance.

    use super::*;

    #[test]
    fn fresh_state_defaults() {
        let r = ReplicationState::new(1, false, 0);
        assert!(!r.is_replica());
        assert!(r.read_only());
        assert!(r.current_upstream().is_none());
        assert_eq!(r.applied_offset_sum(), 0);
        assert!(r.write_denied_reply(|| 0).is_none());
        assert!(r.read_denied_reply().is_none());
        assert!(!r.write_possibly_gated());
        assert!(!r.read_possibly_gated());
    }

    #[test]
    fn applied_offset_sum_is_per_runner_sum_not_max() {
        let r = ReplicationState::new(3, false, 0);
        assert_eq!(r.applied_offset_sum(), 0, "no runners → 0");
        // Simulate a 3-runner fleet's registry (start_runners sizes
        // this in production).
        r.progress.size_runner_slots(3);
        r.progress.record_ping(0, 1, 100, 40);
        r.progress.record_applied(1, 25);
        r.progress.record_applied(2, 35);
        assert_eq!(r.applied_offset_sum(), 100, "sum across runners, not max");
        // Plain store semantics: a resync rewind must show through.
        r.progress.record_applied(1, 5);
        assert_eq!(r.applied_offset_sum(), 80);
        // Out-of-range slot is ignored (registry resize race guard).
        r.progress.record_applied(9, 1_000);
        assert_eq!(r.applied_offset_sum(), 80);
        // stop_runners clears the registry.
        r.stop_runners();
        assert_eq!(r.applied_offset_sum(), 0);
    }

    #[test]
    fn start_runners_before_runtime_wiring_errors() {
        let r = ReplicationState::new(1, false, 0);
        let result = r.start_runners((IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), 6400));
        assert!(result.is_err(), "inboxes never taken → no runtime drains them");
        assert!(!r.is_replica());
    }

    #[test]
    fn promote_bumps_epoch_only_from_replica() {
        let r = ReplicationState::new(1, false, 0);
        r.promote_stop_runners();
        assert_eq!(r.promotion_epoch(), 0, "primary → primary is not a promotion");
        r.force_replica_flag();
        r.promote_stop_runners();
        assert_eq!(r.promotion_epoch(), 1);
        assert!(!r.is_replica());
    }

    #[test]
    fn write_denied_orders_fences() {
        let r = ReplicationState::new(1, false, 0);
        r.set_quiesce(Some("10.0.0.9:7000".into()));
        assert!(r.write_possibly_gated(), "quiesce raises the gate bit");
        let reply = r.write_denied_reply(|| 0).expect("quiesced");
        assert!(reply.starts_with(b"-QUIESCED"), "{}", String::from_utf8_lossy(&reply));
        assert!(r.set_quorum_fence(true), "flag changed");
        let reply = r.write_denied_reply(|| 0).expect("fenced");
        assert!(reply.starts_with(b"-NOREPLICAS"), "fence outranks quiesce");
        assert!(!r.set_quorum_fence(true), "unchanged flag reports false");
        r.set_quorum_fence(false);
        r.set_quiesce(None);
        r.force_replica_flag();
        let reply = r.write_denied_reply(|| 0).expect("read-only replica");
        assert!(reply.starts_with(b"-READONLY"));
        r.set_read_only(false);
        assert!(r.write_denied_reply(|| 0).is_none(), "writable replica");
        assert!(!r.write_possibly_gated(), "writable replica clears the bit");
    }

    #[test]
    fn every_gate_writer_bumps_the_control_epoch() {
        let r = ReplicationState::new(1, false, 0);
        let epoch = r.control_epoch_handle();
        let mut last = epoch.load(Ordering::Acquire);
        let assert_bumped = |last: &mut u64, what: &str| {
            let now = epoch.load(Ordering::Acquire);
            assert!(now > *last, "{what} must bump the control epoch");
            *last = now;
        };
        r.set_read_only(false);
        assert_bumped(&mut last, "set_read_only");
        r.set_min_replicas(2);
        assert_bumped(&mut last, "set_min_replicas");
        r.set_max_staleness_ms(500);
        assert_bumped(&mut last, "set_max_staleness_ms");
        r.set_quiesce(Some("x:1".into()));
        assert_bumped(&mut last, "set_quiesce");
        r.set_quorum_fence(true);
        assert_bumped(&mut last, "set_quorum_fence(changed)");
        assert!(!r.set_quorum_fence(true));
        assert_eq!(epoch.load(Ordering::Acquire), last, "unchanged fence: no bump");
        r.force_replica_flag();
        assert_bumped(&mut last, "force_replica_flag");
        r.stop_runners();
        assert_bumped(&mut last, "stop_runners");
    }

    #[test]
    fn min_replicas_on_primary_keeps_write_gate_raised() {
        let r = ReplicationState::new(1, false, 0);
        r.set_min_replicas(1);
        assert!(r.write_possibly_gated(), "count is dynamic → always re-judge");
        let denied = r.write_denied_reply(|| 0).expect("no healthy replicas");
        assert!(denied.starts_with(b"-NOREPLICAS"));
        assert!(r.write_denied_reply(|| 1).is_none(), "count satisfied");
    }
