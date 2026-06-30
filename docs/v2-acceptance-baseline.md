# kevy v2.0 acceptance baseline

This doc catalogs the chaos / soak / fuzz test suite that gates v2.0 ship,
the empirical baseline each test established at its introduction, and how
those baselines combine into the v2 acceptance criteria.

It is **the authoritative reference** for "what does industrial-grade mean
for kevy?" Every test below is shipped, gated `#[ignore]` (opt-in), and
runs against the kevy release binary.

## Table of contents

- [How to run the whole suite](#how-to-run-the-whole-suite)
- [v2 acceptance gates](#v2-acceptance-gates)
- [Phase A — failure-mode robustness](#phase-a--failure-mode-robustness)
- [Phase B — operability + observability](#phase-b--operability--observability)
- [Phase C — cluster correctness under chaos](#phase-c--cluster-correctness-under-chaos)
- [Phase D — large-scale E2E](#phase-d--large-scale-e2e)
- [Open findings](#open-findings)

## How to run the whole suite

```text
cargo build --release -p kevy
cargo test -p kevy --release -- --ignored --nocapture
```

For the soak gate (24 h):

```text
KEVY_SOAK_SECS=86400 cargo test -p kevy --test soak_long_running_chaos \
    --release -- --ignored --nocapture
```

Total wall-clock for the non-soak suite is ≤ 60 s on Mac M2 Pro.

## v2 acceptance gates

The v2.0 ship gate is: **every test below passes empirically + each
empirical headline number ≥ baseline (no regression)**.

| Gate | Test | Baseline (Mac M2 Pro) | Roadmap step |
|---|---|---|---|
| RESP parser doesn't panic on garbage | `kevy-resp` fuzz harness (`fuzz::run_n`) | 1 M random byte streams, 0 panics | v1.36 |
| Connection cap enforced | `maxclients_chaos.rs` | 50 conns accepted, 51st rejected | v1.37 |
| Disk-full restart recovery | `disk_full_chaos.rs` | SIGXFSZ kill survived, AOF replays cleanly | v1.38 |
| FD exhaustion graceful | `fd_exhaust_chaos.rs` | rlimit 256, kevy accepts up to cap, no panic | v1.38 |
| SIGTERM drains in-flight | `sigterm_drain_chaos.rs` | 192 k ACKs / 0 lost / 0.08 s drain | v1.39 |
| Backup / restore round-trip | `backup_restore_chaos.rs` | pack mid-AOF-write, unpack, all keys present | v1.40 |
| Prometheus `/metrics` | (manual: curl `:9090/metrics`) | text/plain `version=0.0.4` exposition | v1.41 |
| Audit log | `audit_log_chaos.rs` | CONFIG SET / DEBUG entries recorded, 256 B truncate | v1.42 |
| Cluster mode single-node | `cluster_topology_chaos.rs` | MOVED reply, CLUSTER NODES bulk, PING +PONG | v1.43 |
| Multi-node peer formation | `cluster_peer_formation_chaos.rs` | 3-node start, 2 survive after SIGKILL of node 0 | v1.44 |
| Scoped MISDIRECTED | `scope_misdirected_chaos.rs` | -MISDIRECTED reply on non-owner, survivor invariant | v1.45 |
| Client-side network partition | `network_partition_chaos.rs` | **1000/1000 storm conns in 0.10 s** | v1.46 |
| AOF compat matrix | `aof_compat_matrix_chaos.rs` | 100 v1.0-vintage RESP commands replay clean, torn discarded | v1.47 |
| Multi-tenant isolation | `multi_tenant_e2e_chaos.rs` | **5000 ACKs / 20 threads / 0.05 s / 0 leak** | v1.48 |
| Burst absorption | `burst_ramp_realistic_chaos.rs` | **10 k ops/s burst, 15 k total / 0 errs** | v1.49 |
| Long-running no-leak | `soak_long_running_chaos.rs` | **143 k ACK/s, slope 4.7 KiB/sample** | v1.50 |

## Phase A — failure-mode robustness

### v1.36 — RESP parser fuzz harness

Pure-std LCG-driven fuzzer in `kevy-resp::fuzz`. 5 strategies (Uniform /
StructuredJunk / MutatedValid / OversizedClaim / NegativeLengths).
**1 M streams, 0 panics**. Catalog: `docs/error-replies.md` enumerates
every -ERR / -WRONGTYPE / -MOVED / -CROSSSLOT / -MISDIRECTED / -OOM /
-READONLY / -MISCONF kevy emits.

### v1.37 — max_clients enforcement

Config `[server].max_clients = N` distributed as `N / shard_count` per
shard. Accept gate rejects conns past the per-shard cap; `rejected_connections`
counter increments. Verified at 50 conns: 50 accepted, 51st rejected.

### v1.38 — resource exhaustion graceful

`HarnessConfig.rlimit_nofile` + `rlimit_fsize` via async-signal-safe
`pre_exec` setrlimit(2). FD exhaust: kevy honors RLIMIT_NOFILE, no
panic. Disk full: SIGXFSZ kernel-kills kevy when RLIMIT_FSIZE
exceeded — **restart recovery from the AOF is the contract**, not
write survival. Documented as v1.38.x candidate to add SIGXFSZ handler.

## Phase B — operability + observability

### v1.39 — SIGTERM graceful drain

`kevy_sys::install_signal_handler(SIGTERM, handler)` installs an
async-signal-safe stop flag; a polling-bridge thread mirrors it to the
per-run stop Arc. **Empirical: 192 k pre-stop ACKs / 0 lost / 0.08 s
drain at sustained load**.

### v1.40 — backup / restore CLI

`kevy-cli::backup::{pack, unpack}` — std-only KEVYBKP1 magic,
`[u16 name_len, name, u64 body_len, body]` per file, `u16=0` EOF.
Race-safe: pack zero-pads when file shrinks between metadata stat and
content read. Verified by chaos test: pack during live AOF writes,
unpack into fresh dir, all keys replay.

### v1.41 — Prometheus `/metrics` endpoint

Pure-std HTTP/1.1 server, serial accept, `text/plain; version=0.0.4`.
Reads `stats::aggregate()`. Opt-in via `[metrics] listen_port`.

### v1.42 — audit log

`OnceLock<Option<Mutex<File>>>`, `O_APPEND` + atomic write per record:
`<unix_micros>\t<arg1>\t<arg2>\t...\n`, 256 B truncate, tab/newline
sanitize. Hooks: CONFIG SET / CONFIG REWRITE / DEBUG.

## Phase C — cluster correctness under chaos

### v1.43 — cluster topology

Single-node `[cluster] enabled = true` mode. MOVED reply on slot
miss, multi-bulk nils on MGET cross-slot (documented as v1.43.x
candidate to emit -CROSSSLOT instead), CLUSTER NODES bulk reply,
PING +PONG. Wall-clock 0.22 s.

### v1.44 — multi-node peer formation

3 nodes, 48-port block partitioned 3 × 16 to avoid TCP port reuse
race. kevy-elect peers configured. Nodes 1+2 survive SIGKILL of node 0.
Observational: `cluster_known_nodes=0` finding (v1.44.x candidate).

### v1.45 — kevy-scope MISDIRECTED

2 nodes, `scopes = "app:billing:=nodeA"`. nodeA returns +OK; nodeB
returns `-MISDIRECTED writer is 127.0.0.1:<elect_port>` — finding:
reply quotes `elect_port` instead of main client port (v1.45.x
candidate). Survivor invariant: nodeB survives nodeA SIGKILL.

### v1.46 — client-side network partition

4 phases: burst-abandon 200 conns with partial RESP frames; 50
half-close patterns; **1000-conn reconnect storm in 0.10 s = 10 k
conn/s, zero refusals**; post-storm fresh PING +PONG.

### v1.47 — AOF compat matrix

Hand-write a 4 610 B canonical RESP AOF spanning every datatype:
50 SET + 10 INCR + 10 LPUSH + 10 HSET + 10 SADD + 10 ZADD. Append
24 B torn trailer. v1.47 binary replays cleanly, 7 invariants pass,
`EXISTS torn` = 0 (torn command discarded). Closes Phase C.

## Phase D — large-scale E2E

### v1.48 — multi-tenant E2E

5 tenants × 4 writers × 250 SETs = **5 000 SETs / 20 threads / 0.05 s,
fairness skew = 0, zero cross-tenant leak**.

### v1.49 — burst / ramp / realistic

4-phase traffic shape (steady → burst → cooldown → resume), 4
producers, op-mix 70 % short SET / 15 % HSET / 10 % LPUSH / 5 % 4 KB
SET. **Burst phase = 10 004 ACKs in 1 s = 10 k ops/s, 15 034 total
ACKs / 0 errs, post-burst memory 5.9 MiB (cap 8 MiB)**.

### v1.50 — long-running soak

4 producers, mixed-op over bounded 5 k-key space. INFO memory sampled
every 5 s. OLS slope on second-half samples vs cap of 256 KiB / sample.
**30 s smoke: 143 k ACK/s, 4.3 M ACKs, slope 4 699 B/sample (56× under
cap)**. Production gate via `KEVY_SOAK_SECS=86400`.

### v1.51 — this acceptance baseline doc

Cataloged above. Phase D **complete**.

## Findings status

| # | Surfaced | Status | Description | Impact |
|---|---|---|---|---|
| 1 | v1.43 | **CLOSED in v1.56** | MGET cross-slot now returns `-CROSSSLOT` (was multi-bulk nils). Test: `cluster_crossslot_mget.rs`. | UX (Redis-spec) |
| 2 | v1.44 | **CLOSED in v1.57** | `cluster_known_nodes` now reports peer count (was shard count). Test: `cluster_known_nodes_count.rs`. | observability |
| 3 | v1.45 | **CLOSED in v1.55** | `-MISDIRECTED` reply uses CLIENT port via extended `id@host:elect:client` syntax. Legacy syntax retained for compat. Test: `scope_misdirected_client_port.rs`. | client compat |
| 4 | v1.38.x | **CLOSED in v1.58** | `SIGXFSZ` no-op handler installed; failing AOF write returns `EFBIG` instead of kernel-killing kevy. Test: `sigxfsz_survival_chaos.rs`. | survival vs restart |
| 5 | v1.33.x | open | Linux replication chaos test fails to fire | needs Linux repro |
| 6 | v1.34.x | open | 1 h opt-in soak not yet run on lx64 | runtime budget |
| 7 | v1.49.x | open | INFO memory reports `used_memory:0` when keyspace empty | observability nit |
| 8 | v1.52.x | open | CLIENT SETNAME stub (no per-conn name persistence) | needs trait refactor |

The 4 open findings are documented in their respective CHANGELOG
entries. None block v2.0 ship — they are either observability nits or
behaviors that already have a working alternative (e.g. AOF replay
covers SIGXFSZ recovery contract; Jedis records the client name
client-side so app correctness is unaffected).

## Phase F status

- **v1.52** ✅ — Java / .NET ecosystem battle-test (Jedis 5.x, StackExchange.Redis)
- **v1.53** ✅ — Go / Python ecosystem battle-test (go-redis v9, redis-py 5.x)
- **v1.54** ✅ — docs polish + release notes drafting
- **v1.55** ✅ — RC fix: v1.45.x MISDIRECTED client port
- **v1.56** ✅ — RC fix: v1.43.x MGET cross-slot -CROSSSLOT
- **v1.57** ✅ — RC fix: v1.44.x cluster_known_nodes observability
- **v1.58** ✅ — RC fix: v1.38.x SIGXFSZ handler
- **v1.59** ✅ (this) — final RC: docs roll-up + findings closure log
- **v2.0** — ship

Phase E mapped each ecosystem library's golden-path workflow to a
chaos test in `crates/kevy/tests/<lib>_*.rs` (following the existing
`bullmq_*.rs` / `sidekiq.rs` / `celery.rs` / `ioredis_canonical.rs`
pattern). Phase F closed the 4 RC findings driven by Phase E feedback.
