# Vector search (`KIND ann`)

ANN is the index engine's fourth kind: declare it like any index and
the field's raw bytes parse as an f32 vector indexed in a per-shard
HNSW graph, maintained synchronously with every write. There is no
sidecar vector database and no ingestion job: embeddings live in the
same hash rows as the rest of the record, and the graph can never
drift from the data.

```
IDX.CREATE embs ON PREFIX doc: FIELD v TYPE vector KIND ann DIM 768
    [DISTANCE cosine|l2|ip] [M 16] [EF 200]
IDX.QUERY embs KNN <f32-le-blob> LIMIT 10 [EF 400] [FIELDS title]
IDX.REBUILD embs
```

## Quick start (server)

Rows are hash keys under the declared prefix; the indexed value is
one declared field holding the raw vector bytes:

```console
kevy-cli -p 6004 IDX.CREATE embs ON PREFIX doc: FIELD v TYPE vector KIND ann DIM 4
kevy-cli -p 6004 HSET doc:1 title "intro" v "csv:0.1,0.2,0.3,0.4"
kevy-cli -p 6004 HSET doc:2 title "search" v "csv:0.4,0.3,0.2,0.1"
kevy-cli -p 6004 IDX.QUERY embs KNN "csv:0.1,0.2,0.3,0.4" LIMIT 2 FIELDS title
```

The reply is `key, distance` pairs ascending (closest first);
`FIELDS` hydrates hash fields on each hit's owning shard in the same
call. In production the vector value is the binary blob (`dim × 4`
bytes of little-endian f32) written by your client library; the
`csv:` form above is the debugging convenience.

Supporting verbs work like on every kind: `IDX.EXPLAIN embs KNN …`
(parse + plan without execution), `IDX.VERIFY` / `IDX.LIST` (live
stats), `IDX.DROP`. `IDX.REBUILD` is specific to ANN — see
"Deletes and rebuild".

## Quick start (embedded)

The ANN kind is behind the `vector` cargo feature (on by default;
compiled out it answers `KevyError::Unsupported`):

```rust
use kevy_embedded::{AnnSpec, Config, Store};

fn main() -> kevy_embedded::KevyResult<()> {
    let store = Store::open(Config::default())?;

    // m / ef of 0 select the defaults (16 / 200);
    // distance: 0 = cosine, 1 = l2, 2 = ip.
    store.idx_create_ann(b"embs", b"doc:", b"v", AnnSpec {
        dim: 4, distance: 0, m: 0, ef: 0,
    })?;

    let v1: Vec<u8> = [0.1f32, 0.2, 0.3, 0.4]
        .iter().flat_map(|f| f.to_le_bytes()).collect();
    store.hset(b"doc:1", &[
        (b"title".as_slice(), b"intro".as_slice()),
        (b"v".as_slice(), v1.as_slice()),
    ])?;

    // Nearest neighbors ascending: Vec<(key, distance)>.
    // ef = 0 takes the engine default beam width.
    let hits = store.idx_knn(b"embs", &[0.1, 0.2, 0.3, 0.4], 10, 0)?;
    for (key, dist) in &hits {
        println!("{} {dist}", String::from_utf8_lossy(key));
    }
    Ok(())
}
```

`idx_knn` returns `KevyResult<Vec<(Vec<u8>, f32)>>` (`k` clamps to
1..=1000); `KevyError::NotFound` names a missing index,
`KevyError::InvalidInput` a bad `AnnSpec` (zero dim, unknown
distance tag). Embedded builds are synchronous — `idx_create_ann`
returns when the index serves. No `FIELDS` hydration in-process:
read fields with `hget`.

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

## Recall: the EF knob

`EF` (16-4096, default max(4·LIMIT, 100)) is the query beam width —
the canonical HNSW recall/latency knob. Dense near-duplicate regions
need wider beams (measured on a 20k cluster @128d: EF 64 → 0.67
recall@10, 100 → 0.77, 400 → the ≥ 0.90 gate line). Embedded:
`idx_knn(…, ef)` (0 = default).

Recall is gated ≥ 0.90 at EF 400 against exact brute-force ground
truth by [`bench/vectorgate.sh`](../bench/vectorgate.sh) — on a
realistic corpus geometry (an intrinsic-dimension-20 manifold in
128d; ambient-uniform random vectors suffer distance concentration
and represent nothing). If your recall matters, measure it on YOUR
corpus with a brute-force sample exactly the way the gate does, and
tune EF per query, not per index.

## Parameters, deletes, rebuild

`M` (links per node per layer, 4-64) and `EF` (construction beam,
16-1024) are **immutable once created** — changing them means DROP +
re-CREATE. Neighbor selection uses the diversity heuristic (Malkov
Alg. 4), which preserves bridge links to outlying regions.

Deletes tombstone the graph node (it keeps routing, stops matching);
updates tombstone + reinsert. `IDX.VERIFY` reports vectors / bytes /
tombstones and flags `rebuild_recommended` past 30% dead;
`IDX.REBUILD` re-inserts the living per shard (bounded O(n · ef_construction · M · dim) work — every living key goes back through the full HNSW insert, so the neighbour cap M and the dimensionality both multiply in,
answer-preserving — asserted in e2e). Graphs are derived state: never
persisted, rebuilt from data after restart. On the server that
restart rebuild is a background backfill — queries answer
`-INDEXBUILDING` until it completes ([indexes.md](indexes.md));
embedded rebuilds are synchronous.

## Filtering, and hybrid retrieval

No filter-during-search (partition predicates inside the graph walk
are the query-engine slope): KNN first, then hydrate with `FIELDS`
and filter client-side.

The one server-side composition that IS built in: an ANN index and a
text index over the same corpus fuse with reciprocal-rank fusion —

```
IDX.QUERY HYBRID posts MATCH "rust storage" embs KNN <f32-le-blob>
    [LIMIT n] [RRFK k] [EF ef] [FIELDS f…]
```

— rank-based (`Σ 1/(k + rank_i)`, `RRFK` default 60), so BM25 scores
and vector distances need no calibration against each other. See
[text-search.md](text-search.md) for the MATCH half. Embedded
callers run `idx_knn` + `idx_match` and fuse in-process.

**Multi-modal caveat**: navigation graphs degrade on corpora made of
far-separated modes (inter-mode gaps ≫ intra-mode distances, e.g.
two disjoint tenants' embeddings in one index) — late inserts in one
mode can't discover the other, starving the graph of bridges. Real
embedding corpora are continuous manifolds and don't trigger this;
if your data is strongly multi-modal, index the modes separately
(one index per prefix — they're cheap and independent).

## Consistency

Same envelope as every index kind ([indexes.md](indexes.md)): a
write and its graph update are atomic within the owning shard;
cross-shard queries merge per-shard top-k without a global snapshot
(SCAN-class). The catalog persists in a data-dir sidecar; graph
CONTENT is derived state, rebuilt after restart.

## Performance

Measured envelope (receipts in the bench tree):

- [`bench/vectorgate.sh`](../bench/vectorgate.sh) gates KNN LIMIT 10
  p95 < 30ms at 1M × 128d against a real server, with recall ≥ 0.90
  at EF 400 holding simultaneously (both clamps at once — a fast
  wrong answer doesn't pass), plus the memory formula against real
  RSS growth (0.5-1.5×).
- [`bench/PERF-LEDGER.md`](../bench/PERF-LEDGER.md) records the
  comparative shootout: recall-aligned at 1.000, KNN answers in
  0.48 ms vs 0.79 ms — **1.64× ahead** of the RediSearch HNSW on
  the same corpus.

Write-side cost is the standard index tax (one field parse + one
graph insert per matching index per write); construction cost scales
with `EF` (construction beam), which is why it is a declaration-time
parameter.

## Sizing

`bytes ≈ vectors × (dim×4 + 40) + links × 8 + vectors × 32`, reported
live by `IDX.VERIFY`/`IDX.LIST`. Cosine keeps only the normalized
copy (single copy — the raw field bytes stay in the row itself).
1M × 1024d ≈ 4.1 GiB plus links. The gate checks the formula against
real RSS growth (0.5-1.5×).

## See also

- [indexes.md](indexes.md) — the index engine this kind plugs into
  (declaration grammar, backfill contract, consistency envelope).
- [text-search.md](text-search.md) — the BM25 kind and the other
  half of `HYBRID`.
- [verb-reference.md](verb-reference.md) — generated grammar for
  every `IDX.*` form.
- [cookbook.md](cookbook.md) — retrieval recipes in context.
