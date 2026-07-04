# v2.11 validation ledger — every declared line vs measured (2026-07-04)

The five-axis discipline produced one clamp file per train; this
ledger is the cross-train reconciliation the v2.11 arc requires.
All measurements: lx64 (Linux x86_64, io_uring), release-perf,
median-of-connections protocols as defined in each gate.

## Perf lines (gate → declared line → measured)

| Gate | Line | Measured | Margin |
|---|---|---|---|
| perfgate ×7 | ratchet floors (baseline×0.92) | all ✓ (e.g. pinned_cluster_get 30.1M ≥ 27.97M) | ≥8% |
| idxgate | IDX.QUERY p99 < 2ms @1M | 0.36ms (median-conn) | 5.6× |
| viewgate | virtual p99 < 3ms @1M×2 | 0.29ms | 10× |
| viewgate | materialized read < 2ms | 0.22ms | 9× |
| viewgate | write tax (3 idx + 4 top-K views) < 15% | 1.9% steady-state | 7.9× |
| textgate | MATCH p95 < 20ms @1M docs | 17.36ms | 1.15× |
| vectorgate | KNN p95 < 30ms @1M×128d | 6.01ms (EF 400) | 5× |
| vectorgate | recall@10 ≥ 0.90 | 1.000 (manifold corpus) | — |
| topogate | listener GET p99 < 1ms under load | 0.067ms | 15× |
| topogate | idle-listener write tax < 10% | -0.4% (noise) | — |
| onrampgate | import ≥ 200k cmd/s | 1.26M cmd/s | 6.3× |
| onrampgate | delete --rate accuracy ±20% | 20.0075s / 20s | 0.04% |
| servinggate | hydrated row-list p99 < 1ms | 0.190ms (macOS; lx64 in v2.11 log) | 5× |
| servinggate | view page p99 < 1ms | 0.127ms | 7.9× |
| servinggate | write fan-out (2 idx + 1 view) p99 < 200µs | 83µs | 2.4× |

## Memory formulas (declared → measured ratio)

| Subsystem | Formula | Ratio vs real |
|---|---|---|
| scalar index (idxgate D7) | entries×(val+key+48) | within ±20% ✓ |
| view members (viewgate) | order(8)+key+48 per member | 1.00 |
| text inverted (textgate) | Σtoken(len+48)+postings×64+docs×(key+text+72) | 0.54 of RSS growth (band 0.5-1.5) |
| ann graph (vectorgate) | vectors×(dim×4+40)+links×8+vectors×32 | 0.87 of RSS growth |
| memgate | 30M-key envelope | PASS every train |

## Durability / disk (diskgate + chaosfsck + v2.3 contract)

- Restore drill: snapshot + (gen, offset) = exact restore point — diskgate PASS every train.
- AOF rewrite pause: < 2s envelope re-checked at 32M-key mixed load (scalesoak phase 4).
- kill -9 mid-write: replay + sidecar backfill → index/view answers
  IDENTICAL to a fresh drop+recreate rebuild (chaosfsck PASS).
- Migration: kill -9 mid-import → --resume converges to equal digest
  (onrampgate PASS).

## Correctness cross-checks

- Pruned==naive BM25 equivalence (kevy-text unit suite).
- ANN recall vs exact numpy ground truth (vectorgate 1.000).
- PREFIX.DIGEST: shard-count + insert-order invariance; server hex ==
  embedded digest (cross-surface pin).
- OP_TABLE parity CI: every (op, surface) pair present or explicitly
  exempted; covgate ratchet green since v2.1.

## Known documented approximations (not defects)

- BM25 statistics are shard-local (docs/text-search.md).
- Single very-common-term MATCH degrades to a postings scan.
- ANN multi-modal corpora need per-mode indexes (docs/vector-search.md).
- Export is per-key point-in-time (SCAN-class; docs/migration.md).
- Materialized views settle briefly after CREATE (docs/views.md).
