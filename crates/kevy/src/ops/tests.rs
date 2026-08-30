//! Lib tests for the `ops` dispatcher — split out of `mod.rs` to
//! keep that file under the project's 500-LOC ceiling.

use super::dispatch_ops;
use kevy_resp::Argv;
use kevy_store::Store;

fn run(verb: &[u8], rest: &[&[u8]]) -> Vec<u8> {
    run_on(&crate::KevyCommands::new(), verb, rest)
}

fn run_on(c: &crate::KevyCommands, verb: &[u8], rest: &[&[u8]]) -> Vec<u8> {
    let mut a = Argv::default();
    a.push(verb);
    for r in rest {
        a.push(r);
    }
    let mut out = Vec::new();
    let mut store = Store::new();
    let handled = dispatch_ops(&c.ctx(), verb, &mut store, &a, &mut out);
    assert!(handled, "verb {:?} not handled", String::from_utf8_lossy(verb));
    out
}

#[test]
fn info_returns_bulk_with_sections() {
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

/// v4.1-V6 (F16.2): the process gauge beside the store gauge — a
/// running test process is resident, so the probe answers > 0 on
/// every platform kevy ships on.
#[test]
fn info_memory_reports_process_rss() {
    let out = run(b"INFO", &[b"memory"]);
    let s = String::from_utf8(out).unwrap();
    let rss: u64 = s
        .lines()
        .find_map(|l| l.strip_prefix("process_rss_bytes:"))
        .expect("process_rss_bytes line present")
        .trim()
        .parse()
        .expect("a byte count");
    assert!(rss > 0, "the probe must answer on dev platforms, got {rss}");
}

/// v4.1-V6 (the smix ask's server twin): the AOF on-disk format is
/// a readable state. This harness runs no reactor tick, so the
/// gauge holds its default — `off` — which is also the truthful
/// answer for a store with no AOF.
#[test]
fn info_persistence_reports_aof_format() {
    let out = run(b"INFO", &[b"persistence"]);
    let s = String::from_utf8(out).unwrap();
    assert!(s.contains("aof_format:off"), "{s}");
}

#[test]
fn info_replication_master_default_shape() {
    // Default standalone — `current_upstream()` is None → master
    // shape with offset/connected folded from the shard view
    // slots. Per-replica list — 3 fake replica processes,
    // offset=42.
    let ack = |off| Some(kevy_rt::ReplicaAck { acked_offset: off, ack_age_ms: 0 });
    let replicas = vec![
        (
            "kevy-replica-7001#0".to_string(),
            std::net::Ipv4Addr::new(10, 0, 0, 1),
            50_001,
            42u64,
            ack(42),
        ),
        (
            "kevy-replica-7002#0".to_string(),
            std::net::Ipv4Addr::new(10, 0, 0, 2),
            50_002,
            41u64,
            ack(41),
        ),
        (
            "kevy-replica-7003#0".to_string(),
            std::net::Ipv4Addr::new(10, 0, 0, 3),
            50_003,
            40u64,
            ack(40),
        ),
    ];
    let c = crate::KevyCommands::new();
    c.state().obs.publish_repl_view(0, crate::state::ReplShardView { offset: 42, replicas });
    let out = run_on(&c, b"INFO", &[b"replication"]);
    let s = String::from_utf8(out).unwrap();
    assert!(s.contains("role:master"), "got: {s}");
    assert!(s.contains("connected_slaves:3"), "got: {s}");
    assert!(s.contains("master_repl_offset:42"), "got: {s}");
    assert!(s.contains("master_replid:"), "got: {s}");
    // No replica-only fields.
    assert!(!s.contains("master_host"), "got: {s}");
    assert!(!s.contains("master_link_status"), "got: {s}");
    // No cleanup needed: the view lives in this test's own
    // KevyCommands shard zone, not in any shared static.
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

#[test]
fn wait_non_integer_args_error() {
    let out = run(b"WAIT", &[b"x", b"1000"]);
    assert!(out.starts_with(b"-ERR value is not an integer"));
    let out = run(b"WAIT", &[b"1", b"-3"]);
    assert!(out.starts_with(b"-ERR value is not an integer"));
}

#[test]
fn repl_token_on_replica_flag_reports_runner_view() {
    // Dispatch-level REPL.TOKEN (replica path): reads the per-runner
    // gen/applied registries.
    let mut a = Argv::default();
    a.push(b"REPL.TOKEN");
    let mut out = Vec::new();
    let mut store = Store::new();
    let c = crate::KevyCommands::new();
    c.state().replication.force_replica_flag();
    // No runners installed → zero streams → empty array.
    assert!(dispatch_ops(&c.ctx(), b"REPL.TOKEN", &mut store, &a, &mut out));
    assert_eq!(out, b"*0\r\n");
}

#[test]
fn repl_wait_on_primary_is_ok_and_bad_token_errors() {
    assert_eq!(run(b"REPL.WAIT", &[b"1", b"42"]), b"+OK\r\n");
    assert!(run(b"REPL.WAIT", &[b"1"]).starts_with(b"-ERR REPL.WAIT"));
}
