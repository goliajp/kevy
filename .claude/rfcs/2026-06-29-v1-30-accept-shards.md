# RFC v1.30 — `--accept-shards N` static accept-set config (A8 simplified)

**Status**: planning lock 2026-06-29 round 27, implementation starts immediately
**Author**: GOLIA K.K.
**Date**: 2026-06-29
**Anchors**:
- [bench/PERF-FINDING-2026-06-29-fair-core-bigval-SET.md](../../bench/PERF-FINDING-2026-06-29-fair-core-bigval-SET.md) — empirical support: kevy 10c LOSES MORE than kevy 2c on -d 65536 SET (-9% relative throughput) because of conn-density inversion at 10 shards × 5 conns each.
- [bench/PERF-DECOMP-2026-06-28-c100-GET-vs-valkey-9.1.md](../../bench/PERF-DECOMP-2026-06-28-c100-GET-vs-valkey-9.1.md) §"A8 conn-affinity rebalance" — original Top-N attack identification (~40-60 µs/op estimated gain, 200+ LOC, "breaks stateless-shard model").

## Why now

v1.29.0 shipped (round 23) as "architectural prep + empirical Phase A re-verification". The empirical sweep + methodology v1.2 §9 gate compliance establishes: **kevy userspace hot-path is at the architectural ceiling at every measured workload axis except `-d 65536 SET` (and large-payload SET tails by extension)**. The remaining gap is structurally located in the kernel TCP path AND in kevy's many-shard config inverting on sparse-conn workloads (per round 14 fair-core finding).

A8 is the only remaining app-layer attack with empirical support. The "200+ LOC, breaks stateless-shard model" warning came from imagining the most invasive variant (kernel BPF SK_REUSEPORT redirect + fd migration). The simplified static-set variant is much smaller AND preserves the stateless-shard model: each shard still runs the same code; one runtime config flag determines whether a given shard arms accept SQE or not. Off-accept-set shards still receive cross-shard dispatched work via `Inbound::RequestBatch` — they're "compute only" shards, not silenced.

## L1 lock — version line

- This work ships as **v1.30.0** (workspace minor bump). Pure perf/architecture, no API break.
- v1.27 = Lua only, v1.28 = workflow infra, v1.29 = perf architectural prep. **v1.30 = static accept-set for sparse-conn workload perf**. Per the established pattern, subsequent non-accept-set fixes go to v1.30.x patch line.

## Non-goals

- **No dynamic accept-set rebalancing**. Static config only. Dynamic adapts (per-shard density tracking + cross-shard coordination) are v1.31+ if v1.30 ships and is validated.
- **No fd migration / SCM_RIGHTS / IORING_OP_MSG_RING**. Conns stay on their accepting shard for their lifetime. Cross-shard dispatch for other-shard keys uses the existing Inbound channel (no change).
- **No kernel BPF SK_REUSEPORT**. Out of scope; kernel work is D-series.
- **No automatic detection of expected conn count**. User configures `--accept-shards` per workload knowledge.

## What changes

### File 1: `crates/kevy-config/src/lib.rs`

Add `accept_shards: Option<usize>` to the runtime config (TOML-loadable + CLI-overridable). Default `None` = all shards accept (v1.29 byte-identical behavior). `Some(N)` = only shards `0..N` accept; `N..nshards` are compute-only.

### File 2: `crates/kevy-cli/src/main.rs` (and `crates/kevy/src/main.rs` if it owns CLI)

Add `--accept-shards N` flag, threaded through to the runtime. Validation: `N > 0` AND `N <= nshards`. Else error at startup.

### File 3: `crates/kevy-rt/src/runtime.rs` + `crates/kevy-rt/src/shard.rs`

`Shard` gains `arms_accept: bool` field. At Runtime construction, populate as `shard_id < accept_shards` (else `accept_shards.is_none()` = always true).

### File 4: `crates/kevy-rt/src/uring_reactor.rs`

Gate accept SQE arming on `self.arms_accept`. The existing `if !accept_inflight { ring.prep_accept_multishot(...) }` becomes `if self.arms_accept && !accept_inflight { ... }`. Same gate for cluster + UDS listeners.

### File 5: `crates/kevy-rt/src/shard.rs` (epoll path)

Same gate for the epoll reactor's accept loop (`shard.rs::run()` body) so kevy without io_uring also honors the config.

### File 6: docs

- `docs/sharding.md` or new `docs/accept-shards.md`: explain when to use `--accept-shards`.
- README cross-link to bench/PERF-FINDING-2026-06-29-fair-core-bigval-SET.md for empirical rationale.

### Files NOT touched

- kevy-store, kevy-bytes, kevy-resp, kevy-uring — no.
- Conn ownership model — unchanged.
- Cross-shard Inbound channel — unchanged.
- Routing logic — unchanged (other-shard keys still dispatch via Inbound::RequestBatch; the receiving shard executes regardless of whether it's in accept-set).

## Correctness

The accept-set gate is purely a runtime decision per shard. Off-accept-set shards still:
- Run the full reactor loop
- Process cross-shard dispatched commands via `drain_inbound`
- Send replies back to the owning conn's shard via cross-shard `Inbound` reply
- Run periodic ticks (BLOCK timeout, replication, AOF, etc.)
- Drop CPU correctly (`spin_loop` then park) when idle

Cross-shard work distribution is unchanged: 90% of SETs still hash to a non-owning shard regardless of accept-set. The win comes from conns being CONCENTRATED on fewer shards, so each accepting shard has higher conn density → busy-poll body amortizes better → cross-shard channel send overhead is amortized over more events per iter.

## Tests

- Unit: `Runtime::new()` honors `accept_shards = Some(N)` by populating shard `arms_accept` correctly.
- Integration: start kevy with `--accept-shards 3 --threads 10`; spawn 50 conns; verify all 50 land on shards 0-2 (via INFO clients).
- Smoke: ecosystem (BullMQ/Sidekiq) still works — sanity that cross-shard dispatch unchanged.

## Perf validation gate

Re-run `bench/PERF-FINDING-2026-06-29-fair-core-bigval-SET.md` bench at -c 50 -d 65536 SET, kevy --threads 10 --accept-shards 3 taskset 0-9. Target: kevy 10c throughput rises past the kevy 2c baseline (65k SET/s) — i.e., **fold conns into fewer shards reverses the conn-density inversion**.

If perfgate green AND no axis-G/I/A regression at default `--accept-shards = nshards` (no change), ship v1.30.0.

## Implementation order (Cn commits, like v1.29 sprint)

C1 — `kevy-config` add `accept_shards: Option<usize>` field + TOML parse.
C2 — `kevy-cli` / `kevy/src/main.rs` add `--accept-shards N` flag + validation.
C3 — `Runtime` plumb config → `Shard::arms_accept`.
C4 — `uring_reactor.rs` gate accept SQEs on `self.arms_accept`.
C5 — `shard.rs` (epoll) gate accept on `self.arms_accept`.
C6 — `docs/accept-shards.md` + README cross-link.
C7 — lx64 perfgate validation (bench at `--accept-shards 3 --threads 10`).
C8 — workspace bump 1.29.0 → 1.30.0 + tag + ship.

Each commit self-contained; runs `cargo test --workspace --lib` green.

## Risks

- **R1 — Conn distribution under SO_REUSEPORT**: Linux SO_REUSEPORT default does flow-hash. If only N sockets bound to a port, all incoming conns hash among those N. Should be automatic; no SO_REUSEPORT BPF needed. Validate: at startup, only N shards bind to listener fd, rest don't. Actually all shards currently bind the same way; the gate is on whether they ARM accept SQE, not whether they bind. With one socket bound by N+M shards but only N armed, kernel routes to the N armed sockets' accept queues. Verify.
- **R2 — Off-accept-set shards still consume CPU**: they busy-poll waiting for cross-shard work. At `--threads 10 --accept-shards 3`, 7 shards spin on idle most of the time. The v1.29 spin_loop is 25% of run_uring self-time per the §9 gate compliance perf record; off-accept-set shards spend even more on spin_loop. Park-on-idle still happens after URING_SPIN_LIMIT, so CPU% is bounded. But on Linux this may waste cores. Mitigation: A7-style conn-density-aware spin_limit (already attempted, reverted) BUT off-accept-set shards have conn_count=0 → A7 sparse tier kicks in → park earlier. Worth combining if A7 gets re-implemented along with A8. v1.30.0 ships without A7; v1.30.x patch line if needed.
- **R3 — Conn-owning shard receives + dispatches BOTH for its own conns AND cross-shard work for other conns**: this is current behavior (no change). Accepting shards are busier than off-accept-set shards because they own + dispatch own conn work + receive cross-shard work for their keys. v1.30.0 doesn't try to balance these.

## What v1.30.0 does NOT include

- Dynamic accept-set adjustment based on observed conn count.
- Per-shard density tracking + REUSEPORT BPF for kernel-level redirect.
- Conn fd migration between shards.
- A7 (spin_limit re-introduction).
- Documentation of how to pick `accept_shards` value for arbitrary workloads (only the empirical case at -c 50 / -d 65536 SET is documented).

Future v1.30.x can layer dynamic adjust + A7-combined park-on-idle for off-accept-set shards.

## Decision summary

v1.30.0 = `--accept-shards N` runtime config + per-shard `arms_accept` gate. 8-commit chain (C1-C8), ~100-150 LOC across `kevy-config` + `kevy-rt` + `kevy-cli` + 1 doc. Closes (or partially closes) the kevy 10-shard inversion on sparse-conn workloads documented in round 14 fair-core finding. Preserves stateless-shard model (off-accept-set shards run identical code; they're compute-only at runtime, not architecturally distinct).
