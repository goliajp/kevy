//! The `INFO` section builders (split from `mod.rs` for the 500-LOC
//! house rule; behaviour unchanged). Each renders one `# Section`
//! block in the canonical valkey order `build_info_body` walks.

use std::time::SystemTime;

use kevy_config::Config;

use super::{appendfsync_str, eviction_str, memory, replication};
use crate::state::Ctx;

pub(super) fn info_server(cfg: &Config, b: &mut String) {
    let now = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    b.push_str("# Server\r\n");
    b.push_str("redis_version:7.4.0\r\n"); // valkey-compat byte-for-byte sniffing
    b.push_str(&format!("kevy_version:{}\r\n", env!("CARGO_PKG_VERSION")));
    b.push_str("redis_mode:standalone\r\n");
    b.push_str(&format!("process_id:{}\r\n", std::process::id()));
    b.push_str(&format!("tcp_port:{}\r\n", cfg.server.port));
    b.push_str(&format!("server_time_usec:{}\r\n", now * 1_000_000));
    b.push_str("\r\n");
}

pub(super) fn info_clients(cfg: &Config, totals: &crate::state::Totals, b: &mut String) {
    b.push_str("# Clients\r\n");
    // Live client conns summed over every shard's per-tick gauge
    // (stale by at most one tick interval).
    b.push_str(&format!("connected_clients:{}\r\n", totals.clients_connected));
    b.push_str(&format!("maxclients:{}\r\n", cfg.server.max_clients));
    b.push_str("\r\n");
}

pub(super) fn info_memory(cfg: &Config, totals: &crate::state::Totals, b: &mut String) {
    let used = totals.used_memory;
    let peak = totals.used_memory_peak;
    b.push_str("# Memory\r\n");
    b.push_str(&format!("used_memory:{used}\r\n"));
    b.push_str(&format!("used_memory_human:{}\r\n", memory::format_bytes_human(used)));
    b.push_str(&format!("used_memory_peak:{peak}\r\n"));
    b.push_str(&format!("used_memory_peak_human:{}\r\n", memory::format_bytes_human(peak)));
    b.push_str(&format!("maxmemory:{}\r\n", cfg.memory.maxmemory));
    b.push_str(&format!(
        "maxmemory_human:{}\r\n",
        memory::format_bytes_human(cfg.memory.maxmemory)
    ));
    b.push_str(&format!("maxmemory_policy:{}\r\n", eviction_str(cfg.memory.maxmemory_policy)));
    b.push_str(&format!("evicted_keys:{}\r\n", totals.evicted_keys));
    // The process, not the store size containers
    // from RSS — indexes, buffers and allocator overhead live outside
    // `used_memory`. 0 on platforms without a probe.
    b.push_str(&format!("process_rss_bytes:{}\r\n", kevy_sys::process_rss_bytes()));
    b.push_str("\r\n");
}

/// `# Tiering`: the unified-budget
/// gauges summed across shards. Emitted only when tiering is enabled —
/// see the call site's byte-stability note.
pub(super) fn info_tiering(totals: &crate::state::Totals, b: &mut String) {
    let t = &totals.tier;
    b.push_str("# Tiering\r\n");
    b.push_str("tiering_enabled:1\r\n");
    b.push_str(&format!("tier_budget_bytes:{}\r\n", t.budget));
    b.push_str(&format!("tier_effective_target:{}\r\n", t.effective_target));
    b.push_str(&format!("cold_keys:{}\r\n", t.cold_keys));
    b.push_str(&format!("cold_bytes:{}\r\n", t.cold_bytes));
    b.push_str(&format!("stub_bytes:{}\r\n", t.stub_bytes));
    b.push_str(&format!("index_reserved_bytes:{}\r\n", t.reserved_bytes));
    b.push_str(&format!("vlog_size_bytes:{}\r\n", t.vlog_bytes));
    b.push_str(&format!("vlog_live_bytes:{}\r\n", t.vlog_live_bytes));
    b.push_str(&format!("vlog_files:{}\r\n", t.vlog_files));
    b.push_str(&format!("vlog_epoch:{}\r\n", t.vlog_epoch));
    b.push_str(&format!("demotions_total:{}\r\n", t.demotions_total));
    b.push_str(&format!("promotions_total:{}\r\n", t.promotions_total));
    b.push_str(&format!("peek_preads_total:{}\r\n", t.peek_preads_total));
    b.push_str(&format!("batch_submissions_total:{}\r\n", t.batch_submissions_total));
    b.push_str("\r\n");
}

/// `# Allocator`: the accounting identity, summed across the shard
/// heaps that reported. Emitted only when at least one did — with the
/// system allocator there is nothing to say, and an all-zero section
/// would say something false.
///
/// Nine terms, disjoint by construction: every mapped byte is in
/// exactly one of them. `alloc_accounted` is printed beside
/// `alloc_mapped` rather than as a difference, because the two agreeing
/// is the check — a reader compares them instead of trusting a residual
/// somebody else computed.
///
/// This is the instrument the resident-ratio work needed and did not
/// have: the one workload where this allocator loses to glibc could be
/// measured, but not attributed to a term.
pub(super) fn info_allocator(totals: &crate::state::Totals, b: &mut String) {
    // Nothing reported: write nothing. An all-zero `# Allocator` under
    // the system allocator would name an allocator that is not running,
    // and it would change INFO's bytes for every build that does not
    // have this one.
    if totals.alloc_shards == 0 {
        return;
    }
    let a = &totals.alloc;
    b.push_str("# Allocator\r\n");
    b.push_str("allocator_impl:kevy-alloc\r\n");
    // The denominator: these are sums over this many independent heaps.
    b.push_str(&format!("alloc_shards_reporting:{}\r\n", totals.alloc_shards));
    b.push_str(&format!("alloc_mapped:{}\r\n", a.mapped));
    b.push_str(&format!("alloc_accounted:{}\r\n", a.accounted()));
    b.push_str(&format!("alloc_live:{}\r\n", a.live));
    b.push_str(&format!("alloc_rounding:{}\r\n", a.rounding));
    b.push_str(&format!("alloc_cache:{}\r\n", a.cache));
    b.push_str(&format!("alloc_span_free:{}\r\n", a.span_free));
    b.push_str(&format!("alloc_returned:{}\r\n", a.returned));
    b.push_str(&format!("alloc_virgin:{}\r\n", a.virgin));
    b.push_str(&format!("alloc_hysteresis:{}\r\n", a.hysteresis));
    b.push_str(&format!("alloc_segment_overhead:{}\r\n", a.segment_overhead));
    b.push_str(&format!("alloc_large_count:{}\r\n", a.large_count));
    b.push_str(&format!("alloc_spans_assigned:{}\r\n", a.spans_assigned));
    b.push_str("\r\n");
}

pub(super) fn info_persistence(ctx: &Ctx<'_>, cfg: &Config, b: &mut String) {
    // The answering shard's background-persistence view, refreshed by
    // the reactor tick via `Commands::on_persist_stats` into the shard
    // zone. Stale by at most one tick interval.
    let (in_flight, rewrites) = ctx.shard.persist_stats();
    b.push_str("# Persistence\r\n");
    // `loading` = a full-resync snapshot ship is being received (the
    // window where reads answer -LOADING). Startup AOF replay never
    // shows here — it completes before the listener accepts.
    b.push_str(&format!("loading:{}\r\n", i32::from(ctx.state.replication.loading())));
    b.push_str(&format!("aof_enabled:{}\r\n", i32::from(cfg.persistence.aof)));
    b.push_str(&format!("appendfsync:{}\r\n", appendfsync_str(cfg.persistence.appendfsync)));
    // The answering shard's view (each shard persists independently);
    // refreshed per reactor tick, so in-progress flips within ~100 ms of
    // a BGSAVE/BGREWRITEAOF starting or finishing.
    b.push_str(&format!("aof_rewrite_in_progress:{}\r\n", i32::from(in_flight)));
    b.push_str(&format!("aof_rewrites_total:{rewrites}\r\n"));
    // The on-disk format this shard's AOF speaks (the smix
    // ask's server twin): v1 = a pre-4.0 file still being appended, so
    // a 3.x binary swap-back still works; v2 closes that window.
    b.push_str(&format!(
        "aof_format:{}\r\n",
        match ctx.shard.aof_format() {
            1 => "v1",
            2 => "v2",
            _ => "off",
        }
    ));
    b.push_str("aof_last_rewrite_time_sec:-1\r\n");
    info_replay_verdict(ctx, b);
    b.push_str("\r\n");
}

/// Boot-replay verdict (answering shard): bytes the startup replay had
/// to drop (quarantined + truncated), and whether the stop was a
/// corrupt frame. Non-zero dropped bytes = the shard recovered less
/// than its AOF held — alert on it.
fn info_replay_verdict(ctx: &Ctx<'_>, b: &mut String) {
    let (dropped, corrupt) = ctx.shard.replay_report();
    b.push_str(&format!("aof_last_open_dropped_bytes:{dropped}\r\n"));
    b.push_str(&format!("aof_last_open_corrupt:{}\r\n", i32::from(corrupt)));
}

pub(super) fn info_stats(ctx: &Ctx<'_>, totals: &crate::state::Totals, b: &mut String) {
    b.push_str("# Stats\r\n");
    b.push_str(&format!("total_connections_received:{}\r\n", totals.connections_received));
    b.push_str(&format!("total_commands_processed:{}\r\n", totals.commands_processed));
    b.push_str(&format!(
        "instantaneous_ops_per_sec:{}\r\n",
        ctx.state.obs.instantaneous_ops_per_sec(totals.commands_processed)
    ));
    b.push_str(&format!("expired_keys:{}\r\n", totals.expired_keys));
    // Redis reports eviction under `# Stats`; kevy had it only under
    // `# Memory`. Emitted in both so tools reading the Redis location
    // see it (additive — the Memory line stays where it was).
    b.push_str(&format!("evicted_keys:{}\r\n", totals.evicted_keys));
    // Redis's name for it. The count is of DECISIONS to close, which is
    // what makes "the server never noticed the cap" tellable from "it
    // noticed and the close had not reached the client yet".
    b.push_str(&format!(
        "client_query_buffer_limit_disconnections:{}\r\n",
        totals.query_buffer_disconnections
    ));
    // kevy extension: the reactor's single-iteration stall upper
    // bound, as the tick's worst observed lateness (µs). The tailgate
    // reads this for its "reactor single-loop <= 100ms" line.
    b.push_str(&format!("reactor_tick_gap_max_us:{}\r\n", totals.tick_gap_max_us));
    // The gauge above is a high-water mark; this counter is the other
    // half of the pair. One late tick and a chronically starved cadence
    // read the same on the gauge alone — two reads of this counter over
    // a known interval tell them apart.
    b.push_str(&format!("reactor_ticks_total:{}\r\n", totals.ticks_total));
    b.push_str("\r\n");
}

pub(super) fn info_replication(ctx: &Ctx<'_>, b: &mut String) {
    // Live `INFO replication` — reads `current_upstream()` to decide
    // the section shape. The primary half folds every shard's view
    // slot into the instance-wide answer (offset sum, one slaveN line
    // per replica process). The fields mirror Redis 7.x, with one
    // simplification: master_replid is a single zeros-string (no
    // failover ID bookkeeping — kevy-elect keeps its own epoch
    // instead). Link status is heartbeat-derived and the per-replica
    // list is live (see the two halves below).
    b.push_str("# Replication\r\n");
    match ctx.state.replication.current_upstream() {
        Some((host, port)) => info_repl_replica(ctx, b, host, port),
        None => info_repl_master(ctx, b),
    }
    b.push_str("\r\n");
}

/// The replica-side (`role:slave`) half of `INFO replication`.
pub(super) fn info_repl_replica(ctx: &Ctx<'_>, b: &mut String, host: std::net::IpAddr, port: u16) {
    b.push_str("role:slave\r\n");
    b.push_str(&format!("master_host:{host}\r\n"));
    b.push_str(&format!("master_port:{port}\r\n"));
    // Heartbeat-derived truth — link status by
    // ping freshness (<3s), applied offset and frame lag from
    // the runner registry.
    let (up, applied, lag, last_io) = ctx.state.replication.replica_link_view();
    b.push_str(if up { "master_link_status:up\r\n" } else { "master_link_status:down\r\n" });
    b.push_str(&format!("master_last_io_seconds_ago:{last_io}\r\n"));
    b.push_str("master_sync_in_progress:0\r\n");
    b.push_str(if ctx.state.replication.read_only() {
        "slave_read_only:1\r\n"
    } else {
        "slave_read_only:0\r\n"
    });
    b.push_str(&format!("slave_repl_offset:{applied}\r\n"));
    b.push_str(&format!("slave_lag_frames:{lag}\r\n"));
}

/// The primary-side (`role:master`) half of `INFO replication` —
/// instance-wide: every shard's view slot folded into one offset sum
/// and one `slaveN` line per replica process.
pub(super) fn info_repl_master(ctx: &Ctx<'_>, b: &mut String) {
    let views = ctx.state.obs.repl_views();
    let (offset, replicas) = replication::aggregate_replication(&views);
    b.push_str("role:master\r\n");
    b.push_str(&format!("connected_slaves:{}\r\n", replicas.len()));
    // Per-replica truth — port is the replica's advertised client
    // port; sent (pumped) / offset (acked) / lag sum its per-shard
    // streams; a replica missing an ACK on ANY stream is `syncing`.
    for (i, agg) in replicas.iter().enumerate() {
        let acked_v = agg.acked.unwrap_or(0);
        let lag = offset.saturating_sub(acked_v);
        let state = if agg.acked.is_some() { "online" } else { "syncing" };
        b.push_str(&format!(
            "slave{i}:ip={},port={},state={state},offset={acked_v},sent={},lag={lag}\r\n",
            agg.ip, agg.port, agg.sent,
        ));
    }
    b.push_str("master_replid:0000000000000000000000000000000000000000\r\n");
    b.push_str(&format!("master_repl_offset:{offset}\r\n"));
}

pub(super) fn info_cluster(cfg: &Config, b: &mut String) {
    b.push_str("# Cluster\r\n");
    b.push_str(if cfg.cluster.enabled { "cluster_enabled:1\r\n" } else { "cluster_enabled:0\r\n" });
    b.push_str("\r\n");
}

/// `# Modules`: the capability surface of THIS build and configuration,
/// one line per module in Redis's `module:name=…` shape so existing
/// tools parse it. kevy's modules are built in, not loaded — the section
/// answers "what can this server do", not "what was dlopen'd": `alloc`
/// reports the compiled-in allocator, `tiering` its runtime state, and
/// the command surfaces report present-by-construction.
pub(super) fn info_modules(totals: &crate::state::Totals, b: &mut String) {
    b.push_str("# Modules\r\n");
    b.push_str(if cfg!(feature = "kevy-alloc") {
        "module:name=alloc,impl=kevy-alloc\r\n"
    } else {
        "module:name=alloc,impl=system\r\n"
    });
    b.push_str(if totals.tier_enabled {
        "module:name=tiering,status=on\r\n"
    } else {
        "module:name=tiering,status=off\r\n"
    });
    for name in ["indexes", "tables", "views", "text", "vector", "cdc", "pubsub", "lua"] {
        b.push_str(&format!("module:name={name},status=on\r\n"));
    }
    b.push_str("\r\n");
}

pub(super) fn info_keyspace(totals: &crate::state::Totals, b: &mut String) {
    b.push_str("# Keyspace\r\n");
    // Redis omits the `dbN:` line entirely for an empty keyspace. `avg_ttl` is
    // a Redis estimate we don't track; report 0 (its "unknown" value).
    if totals.keys > 0 {
        b.push_str(&format!("db0:keys={},expires={},avg_ttl=0\r\n", totals.keys, totals.expires));
    }
    b.push_str("\r\n");
}

#[cfg(test)]
mod allocator_section_tests {
    use super::info_allocator;
    use crate::state::Totals;

    /// Both sides of the emptiness gate, and the identity in the
    /// rendered bytes. This runs with the `kevy-alloc` feature OFF,
    /// which is how the coverage corpus builds — the section is
    /// compiled either way and only its publisher is gated, so the
    /// renderer must not need the allocator to be testable.
    #[test]
    fn no_section_is_written_when_no_shard_reported() {
        let mut quiet = String::new();
        info_allocator(&Totals::default(), &mut quiet);
        assert!(quiet.is_empty(), "wrote a section for zero heaps: {quiet:?}");
    }

    /// A heap's worth of terms that balance, so the assertions below are
    /// about the rendering rather than about the fixture.
    fn two_reporting_shards() -> Totals {
        let mut t = Totals { alloc_shards: 2, ..Totals::default() };
        t.alloc.mapped = 16_777_216;
        t.alloc.live = 152_610;
        t.alloc.rounding = 3_150;
        t.alloc.span_free = 972_720;
        t.alloc.virgin = 444_384;
        t.alloc.hysteresis = 14_742_208;
        t.alloc.segment_overhead = 262_144;
        t.alloc.returned = 200_000;
        // The terms are a partition, so this has to hold before the
        // rendering is asked about — a fixture that does not balance
        // would make the assertion below pass or fail for the wrong
        // reason.
        assert_eq!(t.alloc.accounted(), t.alloc.mapped, "the fixture must balance first");
        t
    }

    #[test]
    fn the_section_names_every_term_it_summed() {
        let t = two_reporting_shards();
        let mut out = String::new();
        info_allocator(&t, &mut out);
        assert!(out.starts_with("# Allocator\r\n"), "{out}");
        assert!(out.ends_with("\r\n\r\n"), "a section ends with a blank line: {out:?}");
        for line in [
            "alloc_shards_reporting:2\r\n",
            "alloc_mapped:16777216\r\n",
            "alloc_accounted:16777216\r\n",
            "alloc_hysteresis:14742208\r\n",
            "alloc_returned:200000\r\n",
        ] {
            assert!(out.contains(line), "missing {line:?} in:\n{out}");
        }
    }

    /// Read back from the text, not from the struct: the section is what
    /// an operator gets, and a renderer that dropped a term would leave
    /// the struct balanced regardless.
    #[test]
    fn the_printed_terms_sum_to_the_printed_map() {
        let t = two_reporting_shards();
        let mut out = String::new();
        info_allocator(&t, &mut out);
        let sum: u64 = [
            "live",
            "rounding",
            "cache",
            "span_free",
            "returned",
            "virgin",
            "hysteresis",
            "segment_overhead",
        ]
        .iter()
        .map(|k| {
            out.lines()
                .find_map(|l| l.trim_end().strip_prefix(&format!("alloc_{k}:")))
                .unwrap_or_else(|| panic!("no alloc_{k} in:\n{out}"))
                .parse::<u64>()
                .unwrap()
        })
        .sum();
        assert_eq!(sum, t.alloc.mapped, "the printed terms do not sum to the printed map");
    }
}
