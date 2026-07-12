//! kevy's own verbs: secondary indexes, views, the change feed, and migration tooling.
//!
//! One row per dispatch-reachable verb. `complexity` and `compat` were
//! derived by reading THIS engine's implementation — never copied from
//! Redis's documentation, because several of ours genuinely differ (SCAN
//! sweeps in one batch, SPOP is deterministic, LINDEX is O(1) where
//! Redis's quicklist is O(N)).

use super::{VerbMeta, v};
use super::flags::*;

#[rustfmt::skip]
pub(super) const ROWS: &[VerbMeta] = &[
    // ---- index -----------------------------------------------------
    v("IDX.COUNT",   "index", -4, RX, "Count index entries matching a range or equality predicate.", "3.0.0", "IDX.COUNT name RANGE min max | EQ value",
      "O(log N + matched) per shard — NOT an O(1) counter and not a B-tree rank query; it counts the matched range",
      "kevy-only: nearest is FT.SEARCH with LIMIT 0 0"),
    v("IDX.CREATE",  "index", -11, WX, "Declare a secondary index over a key prefix (catalog mutation, sidecar-persisted).", "3.0.0", "IDX.CREATE name ON PREFIX prefix FIELD field TYPE i64|f64|str|vector KIND range|unique|text|ann|agg [MAXMEM bytes] [DIM dim] [DISTANCE cosine|l2|ip] [M m] [EF ef] [GROUPBY field]",
      "O(1) at the call; the build is asynchronous — each shard snapshots its matching keys (O(keys on that shard)) then backfills 2048 rows per tick. Per-row build cost: range/unique O(log N), text O(tokens), vector O(ef_construction * M * dim)",
      "kevy-only: nearest is RediSearch FT.CREATE — comparable declarative index-on-prefix, but RediSearch is a separate module with its own index server; kevy's catalog is in-process and the index is maintained in the write hook"),
    v("IDX.DROP",    "index", 2,  WX, "Drop a declared index (catalog mutation, sidecar-persisted).", "3.0.0", "IDX.DROP name",
      "O(catalog size)",
      "kevy-only: nearest is FT.DROPINDEX"),
    v("IDX.LIST",    "index", 1,  RX, "List declared indexes with per-index build state and stats.", "3.0.0", "IDX.LIST",
      "Not O(1) — per shard, per index it computes live stats: range/unique O(1), text O(distinct tokens + docs), vector O(nodes + links), agg O(groups + rows)",
      "kevy-only: nearest is FT._LIST plus FT.INFO combined"),
    v("IDX.EXPLAIN", "index", -2, RX, "The IDX.QUERY parse without the execution: kind, state, est_rows, and the plan line.", "3.0.0", "IDX.EXPLAIN name RANGE min max|EQ value|MATCH text|KNN vector|GROUPS [args ...]",
      "The plan is parse-only, but est_rows costs the same live stats walk as IDX.LIST",
      "kevy-only: nearest is FT.EXPLAIN, which returns a parsed query tree; ours returns kind, build state, estimated rows and a one-line plan"),
    v("IDX.QUERY",   "index", -4, RX, "Query an index: range/equality, text MATCH, vector KNN, aggregation groups, or a two-index COMPOSE.", "3.0.0", "IDX.QUERY name RANGE min max [LIMIT n] [CURSOR c] [FIELDS field ...] | name EQ value [LIMIT n] [CURSOR c] [FIELDS field ...] | name MATCH text [LIMIT n] [FIELDS field ...] | name KNN vector [LIMIT k] [EF ef] [FIELDS field ...] | name GROUP group | name GROUPS [BY count|sum|min|max] [LIMIT n] | HYBRID text_idx MATCH text ann_idx KNN vector [LIMIT n] [RRFK k] [EF ef] [FIELDS field ...] | COMPOSE AND|OR nameA RANGE min max|EQ value nameB RANGE min max|EQ value [LIMIT n] [CURSOR c] [FIELDS field ...]",
      "Depends on the index kind. range: O(log N + limit). unique: O(log N + hits). full-text: O(sum of the posting lists), MaxScore-pruned. vector ANN: O(dim * (M log N + ef * M)) distance evaluations (HNSW). agg GROUPS: a bounded top-K with 1 to 3 fan-out rounds. HYBRID: both sub-queries at 4x depth, fused by RRF at the origin. COMPOSE: NOT limit-bounded — both leaves are materialised in full, because the result is key-ordered while a segment is value-ordered",
      "kevy-only: nearest is RediSearch FT.SEARCH — genuinely comparable on two axes (our text index is BM25 over inverted segments, our vector index is HNSW, and RediSearch offers both). Differences: no separate module or index server; our BM25 statistics are per-shard, not global; and our query surface is a fixed grammar, not a DSL — no field boosting, no phrase or proximity search, no stemming, no aggregation pipeline"),
    v("IDX.REBUILD", "index", 2,  WX, "Rebuild an ANN index (tombstone compaction).", "3.0.0", "IDX.REBUILD name",
      "Vector indexes only (tombstone compaction). O(V_live * ef_construction * M * dim) distance evaluations, on every shard, synchronously — the most expensive verb in the extension surface",
      "kevy-only: no Redis analogue — RediSearch handles HNSW deletes internally and exposes no rebuild verb"),
    v("IDX.VERIFY",  "index", 2,  RX, "Verify an index: re-read every held entry against its row and report entries/bytes/coerce_failures/duplicates/drift/checked.", "3.0.0", "IDX.VERIFY name",
      "O(N) per shard — every held entry is re-read against its row and re-coerced, which is the point: it makes index drift falsifiable rather than merely asserted",
      "kevy-only: no Redis analogue — RediSearch's index is asynchronously maintained and offers no consistency-verify verb"),
    // ---- view ------------------------------------------------------
    v("VIEW.CREATE", "view", -8, WX, "Declare a composed view over indexes with an ORDER BY (catalog mutation, sidecar-persisted).", "3.0.0", "VIEW.CREATE name QUERY tree ORDER BY index [DESC] [MODE virtual|materialized] [TOPK k] [VIA template] (tree = '( AND|OR|DIFF sub sub )' | 'index RANGE min max' | 'index EQ value')",
      "O(tree size) at the call. A materialized view builds lazily on the next tick (a full eval_tree plus an O(M log M) sort); a virtual view builds nothing, ever",
      "kevy-only: no Redis analogue. A view is neither a stored query nor a join — it is a named composition tree over declared indexes that stores membership and order only, never field values"),
    v("VIEW.DROP",   "view", 2,  WX, "Drop a declared view (catalog mutation, sidecar-persisted).", "3.0.0", "VIEW.DROP name",
      "O(catalog size)",
      "kevy-only"),
    v("VIEW.EXPLAIN", "view", 2, RX, "Explain a view's composition tree with per-leaf cardinalities.", "3.0.0", "VIEW.EXPLAIN name",
      "O(sum over leaves of (log N + matched)) — each leaf's cardinality is a real count, so EXPLAIN on a broad leaf costs as much as counting it",
      "kevy-only: nearest is FT.EXPLAIN, which returns a query tree rather than per-leaf cardinalities"),
    v("VIEW.LIST",   "view", 1,  RX, "List declared views with their mode and shape.", "3.0.0", "VIEW.LIST",
      "O(views) at the origin — the catalog is process-global",
      "kevy-only"),
    v("VIEW.QUERY",  "view", -2, RX, "Page through a view's ordered members, optionally hydrating fields via the VIA template.", "3.0.0", "VIEW.QUERY name [LIMIT n] [CURSOR c] [FIELDS field ...]",
      "virtual: O(log N_order + candidates * leaves) — it streams the ORDER index and probes membership per candidate, stopping at the limit, so it never materialises the member set. materialized: O(log M + limit). Adding FIELDS with VIA costs a SECOND fan-out to hydrate the rows",
      "kevy-only: no Redis analogue. Absent by construction: no joins, no predicates at the view layer, no projection from the view itself, no HAVING, no aggregation"),
    v("VIEW.REBUILD", "view", 2, WX, "Force a rebuild of a materialized view.", "3.0.0", "VIEW.REBUILD name",
      "Materialized only. A full eval_tree over the whole tree plus an O(M log M) sort and re-insert, on every shard, now (not deferred to a tick)",
      "kevy-only"),
    v("VIEW.VERIFY", "view", 2,  RX, "Verify a view and report member/byte statistics.", "3.0.0", "VIEW.VERIFY name",
      "materialized: O(M). virtual: O(sum over leaves of (log N + leaf range) + M) — a full fresh evaluation just to report cardinality, which is the one place a virtual view is the expensive one",
      "kevy-only"),
    // ---- feed ------------------------------------------------------
    v("FEED.READ",   "feed", -4, RX, "Read change-feed frames from one shard past a generation/offset cursor.", "3.0.0", "FEED.READ shard generation offset [COUNT n] [PREFIX prefix ...]",
      "A single shard (the shard is an argument, not a fan-out). O(log B) to locate the start offset in the backlog plus O(count * frame size) to decode and re-encode. COUNT defaults to 256 and is hard-capped at 4096",
      "kevy-only: no clean Redis analogue. Keyspace notifications are fire-and-forget with no cursor and no replay; Redis Streams have cursors but are a stream you must dual-write to, not a feed over your existing keyspace; the replication backlog is what this is built on, but Redis exposes it only to replicas. kevy is deliberately not a broker: no consumer groups, no server-side positions, no global cross-shard order"),
    v("FEED.SHARDS", "feed", 1,  RX, "Return the number of change-feed shards.", "3.0.0", "FEED.SHARDS",
      "O(1), answered locally",
      "kevy-only: Redis has no shard-partitioned change feed to enumerate"),
    v("FEED.TAIL",   "feed", 2,  RX, "Return a shard's current feed generation and next offset.", "3.0.0", "FEED.TAIL shard",
      "O(1) on a single shard",
      "kevy-only: nearest is INFO replication's master_repl_offset, which is not a resumable per-shard CDC cursor"),
    // ---- migration -------------------------------------------------
    v("PREFIX.STATS", "migration", 2, RX, "Report key-count and byte statistics for a key prefix.", "3.0.0", "PREFIX.STATS prefix",
      "O(keys on the shard) — a full keyspace walk on every shard; the prefix cannot be precomputed",
      "kevy-only: nearest is INFO keyspace's db0:keys=N,expires=M — the same two counters, but whole-DB and O(1) off a counter, where ours is per-prefix and therefore O(keyspace)"),
    v("PREFIX.DIGEST", "migration", 2, RX, "Order-insensitive checksum of a prefix's rows for migration verification.", "3.0.0", "PREFIX.DIGEST prefix",
      "O(keys on the shard) to walk plus O(sum of row sizes) to digest, on every shard. Per-row digests XOR together, so the result is order-insensitive and safe across shard counts",
      "kevy-only: nearest is DEBUG DIGEST — comparable in kind (Redis also XORs per-key digests), but Redis's is whole-DB and lives behind DEBUG, while ours is per-prefix (the unit a migration actually cares about) and is a supported verb"),
];
