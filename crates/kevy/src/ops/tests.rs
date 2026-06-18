//! Lib tests for the `ops` dispatcher — split out of `mod.rs` to
//! keep that file under the project's 500-LOC ceiling.

use super::dispatch_ops;
use kevy_resp::Argv;
use kevy_store::Store;

fn run(verb: &[u8], rest: &[&[u8]]) -> Vec<u8> {
        let mut a = Argv::default();
        a.push(verb);
        for r in rest {
            a.push(r);
        }
        let mut out = Vec::new();
        let mut store = Store::new();
        let handled = dispatch_ops(verb, &mut store, &a, &mut out);
        assert!(handled, "verb {:?} not handled", String::from_utf8_lossy(verb));
        out
    }

    #[test]
    fn info_returns_bulk_with_sections() {
        // Same global-state serialisation as `info_replication_master_default_shape`
        // — INFO's replication section now reads live `current_upstream`,
        // so a sibling REPLICAOF lib test mid-flight would make this
        // race.
        let _g = crate::replica_state::TEST_STATE_GUARD
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        crate::replica_state::stop_runners();
        let out = run(b"INFO", &[]);
        let s = String::from_utf8(out).unwrap();
        assert!(s.starts_with('$'), "INFO must reply as bulk string");
        assert!(s.contains("# Server"));
        assert!(s.contains("# Replication"));
        assert!(s.contains("role:master"));
        assert!(s.contains("cluster_enabled:0"));
    }

    #[test]
    fn info_specific_section() {
        let out = run(b"INFO", &[b"memory"]);
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("# Memory"));
        assert!(!s.contains("# Server"));
    }

    #[test]
    fn info_replication_master_default_shape() {
        // Default standalone — `current_upstream()` is None → master
        // shape with offset/connected from the per-shard view.
        // Serialise via the same global guard the ROLE tests use so
        // a concurrent REPLICAOF test doesn't flip the upstream slot
        // mid-read.
        let _g = crate::replica_state::TEST_STATE_GUARD
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        crate::replica_state::stop_runners();
        // T1.28.5: per-replica list — 3 fake replicas, offset=42.
        let replicas = vec![
            (std::net::Ipv4Addr::new(10, 0, 0, 1), 6004, 42u64),
            (std::net::Ipv4Addr::new(10, 0, 0, 2), 6004, 41u64),
            (std::net::Ipv4Addr::new(10, 0, 0, 3), 6004, 40u64),
        ];
        crate::ops::replication::set_replication_view(42, replicas);
        let out = run(b"INFO", &[b"replication"]);
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("role:master"), "got: {s}");
        assert!(s.contains("connected_slaves:3"), "got: {s}");
        assert!(s.contains("master_repl_offset:42"), "got: {s}");
        assert!(s.contains("master_replid:"), "got: {s}");
        // No replica-only fields.
        assert!(!s.contains("master_host"), "got: {s}");
        assert!(!s.contains("master_link_status"), "got: {s}");
        // Cleanup so sibling tests start clean.
        crate::ops::replication::set_replication_view(0, Vec::new());
    }

    #[test]
    fn cluster_info_carries_standalone_markers() {
        let out = run(b"CLUSTER", &[b"INFO"]);
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("cluster_enabled:0"));
        assert!(s.contains("cluster_state:ok"));
    }

    #[test]
    fn cluster_nodes_single_self_entry() {
        // T1.33 made CLUSTER NODES read live `current_upstream`, so
        // a sibling REPLICAOF lib test mid-flight would flip the
        // role flag to `myself,slave` and fail the master assert.
        let _g = crate::replica_state::TEST_STATE_GUARD
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        crate::replica_state::stop_runners();
        let out = run(b"CLUSTER", &[b"NODES"]);
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("myself,master"));
        assert!(s.contains("0-16383"));
    }

    #[test]
    fn debug_sleep_zero_returns_immediately() {
        let out = run(b"DEBUG", &[b"SLEEP", b"0"]);
        assert_eq!(out, b"+OK\r\n");
    }

    #[test]
    fn debug_sleep_small_actually_sleeps() {
        let t = std::time::Instant::now();
        let out = run(b"DEBUG", &[b"SLEEP", b"0.05"]);
        let elapsed = t.elapsed();
        assert!(elapsed.as_millis() >= 40, "expected ≥ 40ms, got {elapsed:?}");
        assert_eq!(out, b"+OK\r\n");
    }

    #[test]
    fn wait_returns_zero_replicas() {
        let out = run(b"WAIT", &[b"3", b"1000"]);
        assert_eq!(out, b":0\r\n");
    }

#[test]
fn wait_wrong_args_errors() {
    let out = run(b"WAIT", &[b"3"]);
    assert!(out.starts_with(b"-ERR"));
}

