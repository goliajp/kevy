# Vector search (`KIND ann`)

ANN is the index engine's fourth kind: declare it like any index and
the field's raw bytes parse as an f32 vector indexed in a per-shard
HNSW graph, maintained synchronously with every write.

```
IDX.CREATE embs ON PREFIX doc: FIELD v TYPE vector KIND ann DIM 768
    [DISTANCE cosine|l2|ip] [M 16] [EF 200]
IDX.QUERY embs KNN <f32-le-blob> LIMIT 10 [EF 400] [FIELDS title]
IDX.REBUILD embs
```

Embedded: `idx_create_ann(name, prefix, field, dim, distance, m, ef)`
+ `idx_knn(name, &[f32], k, ef) -> Vec<(key, distance)>`.

## Wire format

A vector field holds `dim × 4` bytes of little-endian f32 (the
RediSearch convention). Wrong length or non-finite values exclude the
row (counted, visible in VERIFY) — same discipline as scalar coercion
failures. Query vectors use the same format; `csv:1.0,2.5,…` is
accepted for debugging.

## Distances and results

`cosine` (default; vectors are normalized at insert — the stored copy
is unit length), `l2` (squared euclidean), `ip` (negative inner
product). Every metric is oriented "smaller = closer", so cross-shard
results merge with one ascending sort. `LIMIT` caps at 1000; no
cursor (deep ANN pagination is an anti-pattern).

Per-shard graphs are independent (index-follows-key, zero cross-shard
write coordination); a query fans out, takes each shard's top-k, and
merges.

`EF` (16-4096, default max(4·LIMIT, 100)) is the query beam width —
the canonical HNSW recall/latency knob. Dense near-duplicate regions
need wider beams (measured on a 20k cluster @128d: EF 64 → 0.67
recall@10, 100 → 0.77, 400 → the ≥ 0.90 gate line). Embedded:
`idx_knn(…, ef)` (0 = default). Recall is gated ≥ 0.90 at EF 400
against brute-force ground truth by `bench/vectorgate.sh`.

## Parameters, deletes, rebuild

`M` (links per node per layer, 4-64) and `EF` (construction beam,
16-1024) are **immutable once created** — changing them means DROP +
re-CREATE. Neighbor selection uses the diversity heuristic (Malkov
Alg. 4), which preserves bridge links to outlying regions.

Deletes tombstone the graph node (it keeps routing, stops matching);
updates tombstone + reinsert. `IDX.VERIFY` reports vectors / bytes /
tombstones and flags `rebuild_recommended` past 30% dead;
`IDX.REBUILD` re-inserts the living per shard (bounded O(n·EF) work,
answer-preserving — asserted in e2e). Graphs are derived state: never
persisted, rebuilt from data after restart.

No filter-during-search (partition predicates inside the graph walk
are the query-engine slope): KNN first, then hydrate with `FIELDS`
and filter client-side.

## Sizing

`bytes ≈ vectors × (dim×4 + 40) + links × 8 + vectors × 32`, reported
live by `IDX.VERIFY`/`IDX.LIST`. Cosine keeps only the normalized
copy (single copy — the raw field bytes stay in the row itself).
1M × 1024d ≈ 4.1 GiB plus links. The gate checks the formula against
real RSS growth (0.5-1.5×).
