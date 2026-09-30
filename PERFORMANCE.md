# Performance

Published numbers for kevy, each with the command that reproduces it.
Every table names the kevy version and the date it was measured; a table
from an older release is kept here only while no newer measurement of the
same thing exists.

Unless a section says otherwise, numbers come from the reference box: a
16-core Linux x86_64 machine (io_uring), release build, server pinned to
one set of cores and the load generator to a disjoint set, TCP loopback.
Competitor versions are pinned in
[`bench/COMPETITOR-ANCHORS.json`](bench/COMPETITOR-ANCHORS.json), and the
harness refuses to produce a number when a running engine reports a
different version.

When a run looks disturbed — the harness prints how much of the box
other processes used, and says when a load generator was the limit — it
is run again. Two tools measure the server:

- `bash bench/arena.sh <kevy-binary>` — kevy against the pinned
  competitors, three rounds, per-cell medians with a 99 % paired interval
  on each ratio. Run once per release, when numbers are published.
- `bash bench/perfgate.sh compare A B` — two kevy builds on one box in one
  run (a path, or a git rev such as `v6.4.0`, `HEAD` or
  `HEAD+kevy-alloc`), alternating which goes first, reporting throughput,
  instructions, cycles and syscalls per command as B / A with the spread
  across rounds. `bash bench/perfgate.sh callgrind A B` counts instructions
  per command per function under valgrind, which does not depend on what
  else the machine is doing.

## Key-value throughput — 2026-09-07 — kevy 6.3.0

`-c 50 -P 16`, one engine at a time, server on cores 0-7 and client on
8-15. Throughput is read from each server's own command counter over a
timed window (not from `redis-benchmark`'s printed rate), median of 5 per
run, and the per-cell median across three full runs.

| verb | kevy | Redis 8.10.1 | valkey 9.1.2 | Dragonfly 1.40.2 | vs Redis 8.10.1 |
|---|---:|---:|---:|---:|---:|
| GET | 7,489,119 | 5,631,398 | 2,980,764 | 2,845,704 | 1.33x |
| SET | 6,824,662 | 2,567,607 | 1,683,227 | 1,943,358 | 2.66x |
| INCR | 6,753,558 | 3,294,927 | 2,279,738 | 1,953,406 | 2.05x |
| SADD | 6,152,617 | 3,753,131 | 2,214,659 | 1,899,967 | 1.64x |
| HSET | 4,002,580 | 2,966,288 | 1,857,532 | 1,773,498 | 1.35x |
| LPUSH | 3,142,699 | 2,860,306 | 1,859,265 | 1,505,141 | 1.10x |
| ZADD | 3,242,967 | 2,818,626 | 1,786,230 | 1,794,335 | 1.15x |

In every cell, kevy's slowest of the three runs is faster than every
competitor's fastest run. The narrowest margins are LPUSH and ZADD against
Redis (1.08x run-to-run worst case). The largest run-to-run spread was
11.1 % for kevy (SADD), 13.5 % for valkey and 5.6 % for Dragonfly.

Reproduce:

```sh
cargo build --release -p kevy
bash bench/arena.sh target/release/kevy
```

## Transport: TCP loopback and Unix socket — kevy 1.25 (2026-06-22)

Same kevy binary and client, 1 M requests × 10 runs, 2σ-filtered mean.

| workload | kevy TCP | kevy UDS | valkey 9.1 TCP | valkey 9.1 UDS |
|---|--:|--:|--:|--:|
| `-c 1` SET | 94.7 k | 166 k | 62.2 k | 96 k |
| `-c 1` GET | 97.3 k | 168 k | 65.0 k | 106 k |
| `-c 50 -P 16` SET | 2.59 M | 4.11 M | 1.82 M | 1.75 M |
| `-c 50 -P 16` GET | 2.67 M | 4.35 M | 2.68 M | 3.42 M |

Reproduce: `bash bench/v125-precision.sh` (TCP) and
`bash bench/v125-precision-uds.sh` (UDS). See [docs/uds.md](docs/uds.md)
for when a Unix socket is the right transport.

Tail latency on the same box, `-c 50`, 10 KB SET, median of 3: p99
0.48 ms (valkey 0.50 ms), p999 0.54 ms (0.58 ms), max 1.78 ms
(2.18 ms). Reproduce: `bash bench/v125-final-verify.sh`.

## Pub/sub fan-out — kevy 1.25 (2026-06-22)

One publisher flooding one channel, K subscribers, messages delivered per
second, median of 3, 16-byte payloads unless noted.

| scenario | kevy | valkey 9.1 | redis 7.4 | kevy / valkey |
|---|--:|--:|--:|--:|
| subs=10 | 6.38 M | 4.01 M | 6.09 M | 1.59× |
| subs=50 | 23.1 M | 5.11 M | 11.5 M | 4.52× |
| subs=100 | 28.4 M | 5.67 M | 12.0 M | 5.00× |
| subs=200 | 31.3 M | 6.27 M | 11.6 M | 4.98× |
| subs=500 | 31.7 M | 6.13 M | 10.6 M | 5.17× |
| subs=50, 256 B | 7.62 M | 5.53 M | 6.05 M | 1.38× |

At subs=50 with 4 KB payloads kevy delivers 8.9 % more than valkey 9.1
(three-run repeat, 2026-06-29).

Reproduce: `bash bench/pubsub_loopback.sh` (one cell; `SUBS`, `MSGS` and
`SIZE` select it) or `bash bench/v125-final-verify.sh` (the whole sweep).

## Persistence — kevy 1.x (2026-06-07)

SET `-c 50 -P 16`, 2 M requests, 10 shards, NVMe (Samsung 9100 PRO),
epoll reactor.

| `appendfsync` | SET/s |
|---|--:|
| no AOF | 1.48 M |
| `everysec` | 1.42 M |
| `always` | 1.30 M |

`always` fsyncs once per pipelined batch, before that batch's replies are
sent, and stays within 12 % of running without an AOF. This drive has no
power-loss protection and acknowledges `fdatasync` from its cache; on a
drive where `fsync` is a real barrier, `always` costs more.

Snapshot `SAVE` of 356 MB / 1.26 M keys writes at about 1.73 GB/s (Apple
M4 Pro, 14 shards).

Reproduce: set `appendfsync` in the `[persistence]` section of the config
file ([docs/persistence.md](docs/persistence.md)), start kevy, then
`redis-benchmark -p 6004 -t set -c 50 -P 16 -n 2000000`.

## Embedded, in process — kevy 1.22 (2026-06-20)

`kevy-embedded::Store` called directly: no socket, no RESP, no reactor.

| op | ops/s | per op |
|---|--:|--:|
| SET (overwrite) | 7.0 M | 143 ns |
| GET (hit) | 9.0 M | 111 ns |
| GET (miss) | 42.2 M | 24 ns |
| INCR | 5.9 M | 169 ns |
| DEL | 5.5 M | 183 ns |

Reproduce: `cargo run -p kevy-embedded --release --example embed_throughput`.

The same Rust program with only the backend changed (one connection,
sequential, 200 k SET then 200 k GET):

| backend | SET/s | GET/s |
|---|--:|--:|
| kevy embedded | 10.10 M | 13.76 M |
| kevy server (io_uring) | 63.5 k | 64.4 k |
| valkey 9.1 | 54.6 k | 53.8 k |
| redis 7.4 | 62.3 k | 61.7 k |

Reproduce: `cargo run -p kevy-embedded --release --example embed_vs_server -- --kevy-port 7011 --valkey-port 7012 --redis-port 7013 -N 200000`.

## Embedded, against each language's native store (2026-07-24)

Measured on an M-series Mac, so these are relative standings, not
absolute numbers. N = 100 k operations, 200 warm keys, median of 3. Each
cell is `kevy time / peer time`: **below 1 means kevy is faster**. kevy
runs with AOF `everysec`; every peer runs with its per-operation sync
turned off. "cold" is one operation per transaction, "amortized" is one
transaction around all N (kevy has no transactions, so its amortized row
uses the batch call where a binding offers one).

**Node** — kevy-node vs better-sqlite3 13.0.1 (WAL, `synchronous=NORMAL`),
node 26.5. Harness: `bench/embeddedgate/node/bench.js`.

| op \ value size | 16 B | 256 B | 4 KB | 64 KB |
|---|:-:|:-:|:-:|:-:|
| GET | 0.58 | 0.25 | 0.35 | 0.29 |
| SET cold | 0.04 | 0.03 | 0.07 | 0.23 |
| SET amortized (`setMany`) | 0.55 | 0.69 | 1.87 | 3.84 |

**Go** — kevy-go vs bbolt v1.5.0 and badger v4.9.4, go 1.25. Harness:
`bench/embeddedgate/go/run.sh`.

| op \ value size | 16 B | 256 B | 4 KB | 64 KB |
|---|:-:|:-:|:-:|:-:|
| GET cold, vs bbolt | 0.48 | 0.48 | 1.98 | 21.3 |
| SET cold, vs bbolt | 0.02 | 0.03 | 0.15 | 0.64 |
| SET amortized (`SetMany`), vs bbolt | 2.70 | 2.54 | 12.8 | 134 |
| GET cold, vs badger | 0.25 | 0.33 | 0.43 | 1.04 |
| SET cold, vs badger | 0.07 | 0.09 | 0.32 | 0.86 |
| SET amortized, vs badger | 4.65 | 5.48 | 6.54 | 1.07 |

kevy-go's `GetScalar` copies the value across cgo while bbolt returns a
pointer into its mmap. `GetView`, the scoped zero-copy read, takes the
64 KB read from 62× behind bbolt to 1.04× ahead.

**C** — kevy C ABI vs LMDB 0.9.33 (`MDB_NOSYNC`), both reads zero-copy.
Harness: `bench/embeddedgate/c/run.sh`.

| op \ value size | 16 B | 256 B | 4 KB | 64 KB |
|---|:-:|:-:|:-:|:-:|
| GET cold | 0.29 | 0.14 | 0.11 | 0.15 |
| GET amortized | 0.50 | 0.21 | 0.16 | 0.24 |
| SET cold | 0.07 | 0.06 | 0.34 | 2.23 |
| SET amortized | 2.79 | 3.61 | 8.83 | 36.8 |

**C#** — Kevy.Embedded vs LightningDB 0.22.0 (LMDB), .NET 8, both
returning an owned `byte[]` except the zero-copy row (`KevyDb.GetView`).
Harness: `bench/embeddedgate/csharp/run.sh`.

| op \ value size | 16 B | 256 B | 4 KB | 64 KB |
|---|:-:|:-:|:-:|:-:|
| GET cold | 0.18 | 0.19 | 0.61 | 0.92 |
| GET amortized | 0.40 | 0.36 | 0.77 | 1.17 |
| GET zero-copy | 0.56 | 0.40 | 0.09 | 0.01 |
| SET cold | 0.07 | 0.07 | 0.38 | 2.40 |
| SET amortized | 3.08 | 3.60 | 8.18 | 28.5 |

Where kevy loses: bulk writes. Every peer folds N writes into one
transaction commit; kevy appends each write to its log as it happens, so
every acknowledged write is recoverable on its own. Large single writes
(64 KB) also cost kevy a second copy of the value into the log buffer.

## Serving queries vs RediSearch — kevy 3.x (2026-07-05)

200 k seeded documents (Zipf text, 128-dimension vectors, Zipf groups),
kevy with one index per query class against redis-stack 7.4.7 with one
composite `FT` index, 200 queries, median of 5.

| query class | kevy | RediSearch |
|---|--:|--:|
| full text, BM25 top-10 | 330 qps, p95 6.59 ms | 273 qps, p95 6.56 ms |
| vector KNN at recall 1.000 | 0.48 ms | 0.79 ms |
| GROUP BY top-100 | 1.85 ms | 202.9 ms |
| numeric range + hydrate | 0.19 ms | 0.43 ms |

Vector search is compared at equal recall against an exact brute-force
answer, not at equal `EF`. At recall 0.99 the two are within 10 %; below
recall 0.98 RediSearch answers faster (0.11 ms), a latency band kevy's
per-shard fan-out does not reach. GROUP BY is fast in kevy because the
aggregate is maintained at write time rather than computed per query.

Reproduce: `python3 bench/arena_serving.py kevy <port>` and
`python3 bench/arena_serving.py stack <port>`; vectors at equal recall
with `python3 bench/arena_ann.py (kevy|stack|truth) <port>`.

## Text search at 1 M documents — 2026-07-23

p95 in milliseconds for each query form, three rounds, spread under 1 %.

| query form | p95 |
|---|--:|
| single term | 3.19 |
| phrase | 23.5 |
| stored values + `FILTER` | 23.9 |
| SORT / DISTINCT | 31.9 |
| multi-field `IN` | 57.5 |
| prefix | 58.4 |
| `TYPO 1` | 68.1 |

Prefix and typo queries walk the whole term dictionary, so their cost
grows with the number of distinct terms. Reproduce:
`bash bench/textgate.sh target/release/kevy` for the single term, with
`POSITIONS=1`, `VALUES=1`, `ORDER=1`, `FIELDS=1` or `TYPO=1` for the other
forms.

## Declared lines, measured — kevy 2.11 (2026-07-04)

Each feature ships with a line it has to hold; the gate named in the
first column measures it.

| gate | line | measured |
|---|---|---|
| idxgate | `IDX.QUERY` p99 < 2 ms at 1 M rows | 0.36 ms |
| viewgate | virtual view p99 < 3 ms at 1 M × 2 | 0.29 ms |
| viewgate | materialized view read < 2 ms | 0.22 ms |
| viewgate | write tax of 3 indexes + 4 top-K views < 15 % | 1.9 % |
| textgate | `MATCH` p95 < 20 ms at 1 M documents | 17.36 ms |
| vectorgate | KNN p95 < 30 ms at 1 M × 128d | 6.01 ms (EF 400) |
| vectorgate | recall@10 ≥ 0.90 | 1.000 |
| topogate | listener GET p99 < 1 ms under load | 0.067 ms |
| topogate | idle listener write tax < 10 % | within noise |
| onrampgate | import ≥ 200 k commands/s | 1.26 M/s |
| onrampgate | `delete --rate` within ±20 % | 20.0075 s for 20 s |
| servinggate | hydrated row list p99 < 1 ms | 0.190 ms |
| servinggate | view page p99 < 1 ms | 0.127 ms |
| servinggate | write fan-out (2 indexes + 1 view) p99 < 200 µs | 83 µs |

Memory formulas the capacity docs use, against measured RSS growth:

| structure | formula | measured / formula |
|---|---|---|
| scalar index | entries × (key + string value + 82…93) | 1.37 (Linux, release build, 1 M rows; the gate allows 0.9–1.6) |
| view members | 8 + key + 48 per member | 1.00 |
| text index | Σ terms (len + 48) + postings × 64 + docs × (key + text + 72) | 0.54 |
| vector graph | vectors × (dim × 4 + 40) + links × 8 + vectors × 32 | 0.87 |

Durability, checked by the same gates: a snapshot plus a
`(generation, offset)` pair restores to exactly that point; an AOF rewrite
pauses for under 2 s at 32 M keys of mixed load; after `kill -9` in the
middle of writes, replayed indexes and views answer identically to a
fresh rebuild; an import killed midway and resumed with `--resume`
converges to the same digest.

Reproduce: `bash bench/<gate>.sh`, for example `bash bench/idxgate.sh`,
`bash bench/viewgate.sh`, `bash bench/servinggate.sh`,
`bash bench/diskgate.sh`, `bash bench/chaosfsck.sh`.

## React Native pub/sub — 2026-07-15

On device through the Nitro (JSI) binding, publish-and-receive cycles per
second, against the Expo module binding of the same engine.

| platform | operation | Expo module | Nitro |
|---|---|--:|--:|
| Android emulator (x86_64) | `cmd` PING | 143 k/s | 488 k/s |
| Android emulator (x86_64) | pub/sub, 16 B | 48.7 k/s | 485 k/s |
| iOS | `cmd` PING | 179 k/s | 1.89 M/s |
| iOS | `cmd` SET | 173 k/s | 1.64 M/s |
| iOS | pub/sub, batched push | — | 1.39 M/s |

An in-process JavaScript emitter such as mitt never leaves JavaScript and
stays faster (4.5 M/s on the same Android emulator); kevy's numbers
include the crossing into native code on every call. The subscriber
thread parks while idle and uses no CPU.

Reproduce: `bindings/expo/example/pubsubBench.ts` in the Expo example app;
see [bindings/nitro/README.md](bindings/nitro/README.md) for the Nitro
build.
