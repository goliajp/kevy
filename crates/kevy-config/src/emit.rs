//! Schema → TOML serialization: `Config::to_toml_string` (the fixed
//! template `CONFIG REWRITE` falls back to) and the canonical
//! `(section, key, value)` pair list shared with the comment-preserving
//! re-emit in [`crate::preserve`].
//!
//! BOTH serializers must cover EVERY schema section — a section missing
//! here silently drops the user's settings on the next `CONFIG REWRITE`
//! (a fuzz run once found [cluster]/[replication]/[lua]/[metrics]/
//! [audit]/[feed] missing, plus `server.max_clients`). The
//! whole-config round-trip tests below lock the coverage.

use crate::cluster::{PeerEntry, ScopeEntry};
use crate::schema::{Config, LogOutput};

/// The sections emitted from [`canonical_pairs`] rather than the
/// hand-aligned template below — single source of truth for both the
/// template and the preserving splice path.
const SERVICE_SECTIONS: [&str; 7] =
    ["cluster", "replication", "lua", "metrics", "audit", "feed", "tiering"];

impl Config {
    /// Render the current config as a standard-template TOML file —
    /// every field, in stable section/key order, with no comments. Used
    /// by `CONFIG REWRITE` when the comment-preserving splice in
    /// [`crate::preserve`] has no original file to work from; only
    /// then are the user's inline comments lost.
    ///
    /// Round-trips: feeding the output back through [`Self::from_toml_str`]
    /// reconstructs an equivalent `Config` (modulo `source_path`).
    pub fn to_toml_string(&self) -> String {
        let mut out = String::new();
        self.write_toml_storage_sections(&mut out);
        self.write_toml_tuning_sections(&mut out);
        self.write_toml_service_sections(&mut out);
        out
    }

    /// `[server]` / `[persistence]` / `[memory]` — the storage-shaped half
    /// of [`Self::to_toml_string`].
    fn write_toml_storage_sections(&self, out: &mut String) {
        use std::fmt::Write;
        let [a, b, c, d] = self.server.bind;
        let _ = writeln!(out, "[server]");
        let _ = writeln!(out, "bind     = \"{a}.{b}.{c}.{d}\"");
        let _ = writeln!(out, "port     = {}", self.server.port);
        let _ = writeln!(out, "threads  = {}", self.server.threads);
        if let Some(n) = self.server.accept_shards {
            let _ = writeln!(out, "accept_shards = {n}");
        }
        let _ = writeln!(out, "max_clients = {}", self.server.max_clients);
        let _ = writeln!(
            out,
            "data_dir = \"{}\"",
            escape_toml_basic_string(&self.server.data_dir.display().to_string()),
        );
        self.write_toml_persistence_section(out);
        let _ = writeln!(out, "[memory]");
        let _ = writeln!(out, "maxmemory         = {}", self.memory.maxmemory);
        let _ = writeln!(out, "maxmemory_policy  = \"{}\"", self.memory.maxmemory_policy.as_str(),);
    }

    /// The `[persistence]` section — split from
    /// [`Self::write_toml_storage_sections`] when the durability arc's
    /// new rewrite/resync knobs pushed it past the fn-length rule.
    fn write_toml_persistence_section(&self, out: &mut String) {
        use std::fmt::Write;
        let _ = writeln!(out);
        let _ = writeln!(out, "[persistence]");
        let _ = writeln!(out, "aof                          = {}", self.persistence.aof);
        let _ = writeln!(
            out,
            "appendfsync                  = \"{}\"",
            self.persistence.appendfsync.as_str(),
        );
        let _ = writeln!(
            out,
            "auto_aof_rewrite_percentage  = {}",
            self.persistence.auto_aof_rewrite_percentage,
        );
        let _ = writeln!(
            out,
            "auto_aof_rewrite_min_size    = {}",
            self.persistence.auto_aof_rewrite_min_size,
        );
        let _ = writeln!(
            out,
            "auto_aof_rewrite_bytes       = {}",
            self.persistence.auto_aof_rewrite_bytes,
        );
        let _ = writeln!(
            out,
            "auto_aof_rewrite_interval_secs = {}",
            self.persistence.auto_aof_rewrite_interval_secs,
        );
        let _ = writeln!(out, "replay_resync                = {}", self.persistence.replay_resync,);
        let _ = writeln!(out);
    }

    /// `[expiry]` / `[log]` / `[notification]` / `[advanced]` / `[slowlog]`
    /// — the runtime-tuning half of [`Self::to_toml_string`].
    fn write_toml_tuning_sections(&self, out: &mut String) {
        use std::fmt::Write;
        let _ = writeln!(out);
        let _ = writeln!(out, "[expiry]");
        let _ = writeln!(out, "hz       = {}", self.expiry.hz);
        let _ = writeln!(out, "sample   = {}", self.expiry.sample);
        let _ = writeln!(out);
        let _ = writeln!(out, "[log]");
        let _ = writeln!(out, "level    = \"{}\"", self.log.level.as_str());
        let _ = writeln!(
            out,
            "output   = \"{}\"",
            escape_toml_basic_string(&self.log.output.as_str()),
        );
        let _ = writeln!(out);
        let _ = writeln!(out, "[notification]");
        let _ = writeln!(
            out,
            "notify_keyspace_events = \"{}\"",
            escape_toml_basic_string(&self.notification.notify_keyspace_events),
        );
        let _ = writeln!(out);
        let _ = writeln!(out, "[advanced]");
        let _ = writeln!(out, "spin_limit       = {}", self.advanced.spin_limit);
        let _ = writeln!(out, "park_timeout_ms  = {}", self.advanced.park_timeout_ms);
        let _ = writeln!(out, "tick_check_every = {}", self.advanced.tick_check_every);
        let _ = writeln!(out, "ring_capacity    = {}", self.advanced.ring_capacity);
        let _ = writeln!(out);
        let _ = writeln!(out, "[slowlog]");
        let _ = writeln!(out, "slower_than_micros = {}", self.slowlog.slower_than_micros,);
        let _ = writeln!(out, "max_len            = {}", self.slowlog.max_len);
    }

    /// `[cluster]` / `[replication]` / `[lua]` / `[metrics]` / `[audit]`
    /// / `[feed]` — emitted from [`canonical_pairs`] so this list can
    /// never drift from the preserving path.
    fn write_toml_service_sections(&self, out: &mut String) {
        use std::fmt::Write;
        let pairs = canonical_pairs(self);
        for section in SERVICE_SECTIONS {
            let _ = writeln!(out);
            let _ = writeln!(out, "[{section}]");
            for p in pairs.iter().filter(|p| p.section == section) {
                let _ = writeln!(out, "{} = {}", p.key, p.value);
            }
        }
    }
}

/// Escape a string for use inside a TOML basic (double-quoted) string.
/// `\` and `"` need backslash escape; other ASCII passes through. The
/// values we emit (paths, enum names) never contain control characters,
/// so this is sufficient for our serialiser.
fn escape_toml_basic_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            other => out.push(other),
        }
    }
    out
}

// ───────────── canonical schema → TOML pairs ─────────────

/// One `(section, key, rendered value)` triple — the unit both the
/// preserving splice ([`crate::preserve`]) and the service-section
/// template above consume.
pub(crate) struct CanonicalPair {
    pub(crate) section: &'static str,
    pub(crate) key: &'static str,
    pub(crate) value: String,
}

/// Every schema field as a canonical pair, in stable section/key order.
/// Optional fields (`server.accept_shards`, `replication.upstream`)
/// are skipped when unset so their absence round-trips to the default.
pub(crate) fn canonical_pairs(cfg: &Config) -> Vec<CanonicalPair> {
    let mut v = Vec::with_capacity(40);
    push_server(&mut v, cfg);
    push_persistence(&mut v, cfg);
    push_memory(&mut v, cfg);
    push_expiry(&mut v, cfg);
    push_log(&mut v, cfg);
    push_notification(&mut v, cfg);
    push_advanced(&mut v, cfg);
    push_slowlog(&mut v, cfg);
    push_cluster(&mut v, cfg);
    push_replication(&mut v, cfg);
    push_lua(&mut v, cfg);
    push_metrics(&mut v, cfg);
    push_audit(&mut v, cfg);
    push_feed(&mut v, cfg);
    push_tiering(&mut v, cfg);
    v
}

// many_single_char_names: a/b/c/d are the four IPv4 octets — positional names
// are clearer than invented words here (`v` is the shared push_* out-param).
#[allow(clippy::many_single_char_names)]
fn push_server(v: &mut Vec<CanonicalPair>, cfg: &Config) {
    let [a, b, c, d] = cfg.server.bind;
    push(v, "server", "bind", format!("\"{a}.{b}.{c}.{d}\""));
    push(v, "server", "port", cfg.server.port.to_string());
    push(v, "server", "threads", cfg.server.threads.to_string());
    if let Some(n) = cfg.server.accept_shards {
        push(v, "server", "accept_shards", n.to_string());
    }
    push(v, "server", "max_clients", cfg.server.max_clients.to_string());
    push(v, "server", "data_dir", toml_string(&cfg.server.data_dir.display().to_string()));
}

fn push_persistence(v: &mut Vec<CanonicalPair>, cfg: &Config) {
    let p = &cfg.persistence;
    push(v, "persistence", "aof", p.aof.to_string());
    push(v, "persistence", "appendfsync", toml_string(p.appendfsync.as_str()));
    push(
        v,
        "persistence",
        "auto_aof_rewrite_percentage",
        p.auto_aof_rewrite_percentage.to_string(),
    );
    push(v, "persistence", "auto_aof_rewrite_bytes", p.auto_aof_rewrite_bytes.to_string());
    push(v, "persistence", "replay_resync", p.replay_resync.to_string());
    push(
        v,
        "persistence",
        "auto_aof_rewrite_interval_secs",
        p.auto_aof_rewrite_interval_secs.to_string(),
    );
    push(v, "persistence", "auto_aof_rewrite_min_size", p.auto_aof_rewrite_min_size.to_string());
}

fn push_memory(v: &mut Vec<CanonicalPair>, cfg: &Config) {
    push(v, "memory", "maxmemory", cfg.memory.maxmemory.to_string());
    push(v, "memory", "maxmemory_policy", toml_string(cfg.memory.maxmemory_policy.as_str()));
}

fn push_expiry(v: &mut Vec<CanonicalPair>, cfg: &Config) {
    push(v, "expiry", "hz", cfg.expiry.hz.to_string());
    push(v, "expiry", "sample", cfg.expiry.sample.to_string());
}

fn push_log(v: &mut Vec<CanonicalPair>, cfg: &Config) {
    push(v, "log", "level", toml_string(cfg.log.level.as_str()));
    push(v, "log", "output", toml_string(&log_output_str(&cfg.log.output)));
}

fn push_notification(v: &mut Vec<CanonicalPair>, cfg: &Config) {
    push(
        v,
        "notification",
        "notify_keyspace_events",
        toml_string(&cfg.notification.notify_keyspace_events),
    );
}

fn push_advanced(v: &mut Vec<CanonicalPair>, cfg: &Config) {
    let a = &cfg.advanced;
    push(v, "advanced", "spin_limit", a.spin_limit.to_string());
    push(v, "advanced", "park_timeout_ms", a.park_timeout_ms.to_string());
    push(v, "advanced", "tick_check_every", a.tick_check_every.to_string());
    push(v, "advanced", "ring_capacity", a.ring_capacity.to_string());
}

fn push_slowlog(v: &mut Vec<CanonicalPair>, cfg: &Config) {
    push(v, "slowlog", "slower_than_micros", cfg.slowlog.slower_than_micros.to_string());
    push(v, "slowlog", "max_len", cfg.slowlog.max_len.to_string());
}

fn push_cluster(v: &mut Vec<CanonicalPair>, cfg: &Config) {
    let cl = &cfg.cluster;
    push(v, "cluster", "enabled", cl.enabled.to_string());
    push(v, "cluster", "port_base", cl.port_base.to_string());
    push(v, "cluster", "node_id", toml_string(&cl.node_id));
    push(v, "cluster", "elect_port_base", cl.elect_port_base.to_string());
    let peers: Vec<String> = cl.peers.iter().map(PeerEntry::to_token).collect();
    push(v, "cluster", "peers", toml_array(&peers));
    let scopes: Vec<String> = cl.scopes.iter().map(ScopeEntry::to_token).collect();
    push(v, "cluster", "scopes", toml_array(&scopes));
}

fn push_replication(v: &mut Vec<CanonicalPair>, cfg: &Config) {
    let r = &cfg.replication;
    push(v, "replication", "role", toml_string(r.role.as_str()));
    if let Some(up) = &r.upstream {
        push(v, "replication", "upstream", toml_string(up));
    }
    push(v, "replication", "listen_port_base", r.listen_port_base.to_string());
    push(v, "replication", "replication_buffer_size", r.replication_buffer_size.to_string());
    push(v, "replication", "reconnect_window_ms", r.reconnect_window_ms.to_string());
    push(v, "replication", "min_replicas_to_write", r.min_replicas_to_write.to_string());
    push(v, "replication", "min_replicas_max_lag_ms", r.min_replicas_max_lag_ms.to_string());
    push(v, "replication", "replica_max_staleness_ms", r.replica_max_staleness_ms.to_string());
    push(v, "replication", "replica_read_only", r.replica_read_only.to_string());
    push(v, "replication", "single_source", r.single_source.to_string());
}

fn push_lua(v: &mut Vec<CanonicalPair>, cfg: &Config) {
    push(v, "lua", "time_limit_ms", cfg.lua.time_limit_ms.to_string());
    push(v, "lua", "allow_dialects", toml_array(&cfg.lua.allow_dialects));
}

fn push_metrics(v: &mut Vec<CanonicalPair>, cfg: &Config) {
    push(v, "metrics", "listen_port", cfg.metrics.listen_port.to_string());
}

fn push_audit(v: &mut Vec<CanonicalPair>, cfg: &Config) {
    push(v, "audit", "log_path", toml_string(&cfg.audit.log_path.display().to_string()));
}

fn push_feed(v: &mut Vec<CanonicalPair>, cfg: &Config) {
    push(v, "feed", "enabled", cfg.feed.enabled.to_string());
    push(v, "feed", "feed_buffer_size", cfg.feed.feed_buffer_size.to_string());
}

/// `[tiering]` — both keys optional: absent = tiering off, and their
/// absence must round-trip to the off default.
fn push_tiering(v: &mut Vec<CanonicalPair>, cfg: &Config) {
    if let Some(budget) = cfg.tiering.budget {
        push(v, "tiering", "budget", toml_string(&budget.as_config_string()));
    }
    if let Some(dir) = &cfg.tiering.spill_dir {
        push(v, "tiering", "spill_dir", toml_string(&dir.display().to_string()));
    }
}

fn push(v: &mut Vec<CanonicalPair>, section: &'static str, key: &'static str, value: String) {
    v.push(CanonicalPair { section, key, value });
}

fn log_output_str(o: &LogOutput) -> String {
    o.as_str().into_owned()
}

/// `["a", "b"]` — the canonical TOML form for a list.
///
/// Emitting `"a,b"` instead would mean a user who wrote an array got a string
/// back the first time anything rewrote their config. The parser still reads
/// both; only one of them is worth writing.
fn toml_array(items: &[String]) -> String {
    let mut out = String::from("[");
    for (i, it) in items.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        out.push_str(&toml_string(it));
    }
    out.push(']');
    out
}

fn toml_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    out.push_str(&escape_toml_basic_string(s));
    out.push('"');
    out
}

#[cfg(test)]
#[path = "emit_tests.rs"]
mod tests;
