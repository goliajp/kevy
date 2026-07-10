//! Round-trip lock for BOTH serializers (template + preserving): every
//! schema section, every field non-default. A field missing from either
//! serializer makes the whole-config equality below fail — this is the
//! regression net for the 2026-07-10 fuzz finding (six whole sections
//! plus `server.max_clients` were silently dropped by `CONFIG REWRITE`).

use std::path::PathBuf;

use crate::cluster::{PeerEntry, ScopeEntry};
use crate::replication::ReplicationRole;
use crate::schema::{AppendFsync, Config, EvictionPolicy, LogLevel, LogOutput};

/// A config where EVERY field differs from `Config::default()` (except
/// `source_path`, which is not part of the wire format).
fn all_fields_non_default() -> Config {
    let mut cfg = Config::default();
    cfg.server.bind = [10, 1, 2, 3];
    cfg.server.port = 7011;
    cfg.server.threads = 6;
    cfg.server.accept_shards = Some(2);
    cfg.server.max_clients = 555;
    cfg.server.data_dir = PathBuf::from("/var/lib/kevy");
    cfg.persistence.aof = false;
    cfg.persistence.appendfsync = AppendFsync::Always;
    cfg.persistence.auto_aof_rewrite_percentage = 250;
    cfg.persistence.auto_aof_rewrite_min_size = 1024;
    cfg.memory.maxmemory = 7 * 1024 * 1024;
    cfg.memory.maxmemory_policy = EvictionPolicy::VolatileLru;
    cfg.expiry.hz = 55;
    cfg.expiry.sample = 9;
    cfg.log.level = LogLevel::Error;
    cfg.log.output = LogOutput::File(PathBuf::from("/var/log/kevy.log"));
    cfg.notification.notify_keyspace_events = "KEA".into();
    cfg.advanced.spin_limit = 512;
    cfg.advanced.park_timeout_ms = 25;
    cfg.advanced.tick_check_every = 64;
    cfg.advanced.ring_capacity = 2048;
    cfg.slowlog.slower_than_micros = 10_000;
    cfg.slowlog.max_len = 64;
    cfg.cluster.enabled = true;
    cfg.cluster.port_base = 7100;
    cfg.cluster.node_id = "node-a".into();
    cfg.cluster.elect_port_base = 7300;
    cfg.cluster.peers =
        PeerEntry::parse_list("node-a@10.0.0.1:7300:7011,node-b@10.0.0.2:7300").unwrap();
    cfg.cluster.scopes = ScopeEntry::parse_list("app:billing:=node-a|node-b").unwrap();
    cfg.replication.role = ReplicationRole::Replica;
    cfg.replication.upstream = Some("10.0.0.1:6004".into());
    cfg.replication.listen_port_base = 16_004;
    cfg.replication.replication_buffer_size = 1024 * 1024;
    cfg.replication.reconnect_window_ms = 5_000;
    cfg.replication.min_replicas_to_write = 1;
    cfg.replication.min_replicas_max_lag_ms = 2_000;
    cfg.replication.replica_max_staleness_ms = 3_000;
    cfg.replication.replica_read_only = false;
    cfg.replication.single_source = true;
    cfg.lua.time_limit_ms = 900;
    cfg.lua.allow_dialects = vec!["5.1".into(), "5.4".into()];
    cfg.metrics.listen_port = 9100;
    cfg.audit.log_path = PathBuf::from("/var/log/kevy-audit.log");
    cfg.feed.enabled = true;
    cfg.feed.feed_buffer_size = 32 * 1024 * 1024;
    cfg
}

#[test]
fn template_round_trips_every_section() {
    let original = all_fields_non_default();
    let toml_text = original.to_toml_string();
    let reparsed = Config::from_toml_str(&toml_text, None).unwrap_or_else(|e| {
        panic!("to_toml_string output did not reparse: {e}\n--- TOML ---\n{toml_text}")
    });
    assert_eq!(original, reparsed, "--- TOML ---\n{toml_text}");
}

#[test]
fn template_round_trips_defaults() {
    // Optional fields (accept_shards, upstream) must stay absent.
    let original = Config::default();
    let toml_text = original.to_toml_string();
    assert!(!toml_text.contains("accept_shards"));
    assert!(!toml_text.contains("upstream"));
    let reparsed = Config::from_toml_str(&toml_text, None).expect("reparse");
    assert_eq!(original, reparsed);
}

#[test]
fn preserving_round_trips_every_section() {
    // Same lock for the comment-preserving splice path: starting from
    // an EMPTY source, every pair lands in the orphan-append and the
    // result must reparse to the identical config.
    let original = all_fields_non_default();
    let toml_text = original.to_toml_string_preserving("").unwrap();
    let reparsed = Config::from_toml_str(&toml_text, None).unwrap_or_else(|e| {
        panic!("preserving output did not reparse: {e}\n--- TOML ---\n{toml_text}")
    });
    assert_eq!(original, reparsed, "--- TOML ---\n{toml_text}");
}

#[test]
fn rewrite_keeps_cluster_enabled() {
    // The original fuzz repro: `[cluster] enabled = true` parsed,
    // serialized to NOTHING, and reparsed as enabled = false.
    let cfg = Config::from_toml_str("[cluster]\nenabled = true\n", None).unwrap();
    assert!(cfg.cluster.enabled);
    let re = Config::from_toml_str(&cfg.to_toml_string(), None).unwrap();
    assert!(re.cluster.enabled, "CONFIG REWRITE dropped [cluster]");
    assert_eq!(cfg, re);
}

#[test]
fn preserving_updates_service_section_values_in_place() {
    // A hand-written [feed] line must be spliced with the live value,
    // not passed through stale (the preserving path shares
    // canonical_pairs with the template, so this locks that list too).
    let src = "[feed]\nenabled = false # cdc\n";
    let mut cfg = Config::from_toml_str(src, None).unwrap();
    cfg.feed.enabled = true;
    let out = cfg.to_toml_string_preserving(src).unwrap();
    assert!(out.contains("enabled = true # cdc"), "--- out ---\n{out}");
}
