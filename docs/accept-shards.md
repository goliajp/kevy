# `--accept-shards N` — static accept-set for sparse-conn workloads

> **v1.30 feature.** Default behavior (no flag) is byte-identical to v1.29:
> every shard arms accept SQE, kernel SO_REUSEPORT spreads new conns
> uniformly across all shards.

## When to use

Each kevy shard runs its own reactor loop with a busy-poll body. For the
busy-poll body to amortize, each shard wants a roughly steady stream of
events per iteration. Below a critical conn-density floor (~5-10
conns/shard on the c100 GET hot path), the body runs many idle iters
between productive iters, and the per-iter overhead (waker pipe drain,
inbox check, accept SQE arm) dominates throughput.

When the workload is **sparse** (few concurrent client conns) but kevy
is configured with **many shards** (one per core for compute), this
inversion can leave kevy slower than a tuned single-server competitor
that uses one or two I/O threads.

Empirical case ([`bench/PERF-FINDING-2026-06-29-fair-core-bigval-SET.md`](../bench/PERF-FINDING-2026-06-29-fair-core-bigval-SET.md)):

> kevy 10-shard vs valkey 10c at `-c 50 -d 65536 -t set`:
> kevy 59.3k SET/s vs valkey 68.5k SET/s — **-13.4 % gap**.
>
> kevy 2-shard same workload: 62.7k SET/s — **kevy 10c LOSES MORE
> than kevy 2c** because of the conn-density inversion (5 conns/shard
> at 10-shard, 25 conns/shard at 2-shard).

The fix is `--accept-shards 3 --threads 10`: fold all 50 conns onto
shards 0-2 (16.7 conns/shard), keep shards 3-9 as **compute-only**
(no accept armed, but still execute cross-shard dispatched commands
via the existing `Inbound::RequestBatch` channel + reply back to the
owning conn shard).

## CLI / TOML / env

CLI (highest priority):
```sh
kevy --port 6004 --threads 10 --accept-shards 3
```

TOML:
```toml
[server]
threads = 10
accept_shards = 3
```

Env:
```sh
KEVY_THREADS=10 KEVY_ACCEPT_SHARDS=3 kevy --port 6004
```

Validation: `accept_shards` must be in `1..=threads`. Otherwise kevy
exits with code 2 at startup.

Default: unset = `None` = every shard accepts (v1.29 byte-identical).

## How it works

1. Runtime construction sets each `Shard.arms_accept` based on the
   config: shards `0..N` get `true`, shards `N..nshards` get `false`.
2. In the reactor loop body (uring or epoll), the accept SQE arm /
   poller-add for the data + cluster + UDS listeners is gated on
   `self.arms_accept`. Off-accept-set shards skip every accept arm.
3. Linux SO_REUSEPORT routes new conns only to the **armed** subset
   of sockets bound to the listener fd. Conns hash-distribute across
   shards `0..N` (uniform under the kernel default routing).
4. Off-accept-set shards run an otherwise-identical reactor loop:
   they receive cross-shard dispatched commands via
   `drain_inbound_core`, execute them against their own keyspace
   slice, send replies back to the owning conn's shard via the same
   cross-shard channel. They run periodic ticks (TTL reaper,
   replication, AOF, etc.) — they're **compute-only**, not silent.
5. Conn ownership is unchanged for a conn's lifetime. A conn that
   lands on shard 1 stays on shard 1 until it closes.

## What it doesn't do

- **No fd migration.** Conns don't move between shards after accept.
- **No dynamic accept-set rebalancing.** Static config only. If the
  workload's conn count changes, restart with a different
  `--accept-shards N`.
- **No kernel BPF SK_REUSEPORT.** Stays in user-space app code.
- **No automatic detection.** User configures `--accept-shards`
  per workload knowledge.

For workloads where the conn count is **unknown** or **changing**,
leave `--accept-shards` unset; v1.30's default behavior matches v1.29.

## Picking the value

Heuristic: `accept_shards ≈ ceil(expected_concurrent_conns / 15)`.

The "15 conns/shard" target is where the busy-poll body's per-iter
overhead amortizes well at a Redis-style workload mix. Tune up for
heavier per-cmd workloads (large values, many cross-shard hops),
down for lighter (small int values, single-shard reads).

For a typical Redis client at `-c 50`, `--accept-shards 3` gives
~16.7 conns/shard. At `-c 100`, `--accept-shards 6` gives ~16.7
conns/shard. At `-c 10` (lightly-multiplexed app), `--accept-shards 1`
puts every conn on shard 0 — the per-shard inbox cost is fully
amortized away. The compute-only shards still service cross-shard
hops for the keys they own.

## Off-accept-set shard CPU

Off-accept-set shards still busy-poll (waiting for cross-shard work)
and then park after `URING_SPIN_LIMIT` idle iters via
`io_uring_enter(wait_nr=1)`. CPU footprint per off-accept-set shard
is bounded by the spin → park ladder. On heavily-quiet conn-share
shards, this is functionally equivalent to "quiet shard at 100 %
once per `URING_SPIN_LIMIT × per-iter-ns` window".

If you need to free those cores entirely, set `--threads N` to match
`--accept-shards` exactly. Compute-only shards exist BECAUSE the
keyspace is sharded across all `--threads` — reducing shards reduces
keyspace parallelism (which is the original reason `--threads N >
accept-shards` is useful at all).

## Benchmark

See [`bench/PERF-FINDING-2026-06-29-fair-core-bigval-SET.md`](../bench/PERF-FINDING-2026-06-29-fair-core-bigval-SET.md)
for the empirical motivation (kevy 10c LOSES MORE than 2c at
`-c 50 -d 65536 SET`).

v1.30 perfgate validation TBD — pending lx64 run. Once landed:
`bench/PERF-FINDING-2026-06-29-v1-30-accept-shards-bench.md`.
