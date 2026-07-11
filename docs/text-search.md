# Full-text search (`KIND text`)

Text is the index engine's third kind: declare it like any index and
the field's raw bytes tokenize into a per-shard inverted segment,
maintained synchronously with every write (zero drift, same
derived-by-construction discipline as `range`/`unique`). There is no
separate search server, no ingestion pipeline, and no analyzer
configuration — one verb declares the index, and every subsequent
write keeps it exact.

```
IDX.CREATE posts ON PREFIX post: FIELD body TYPE str KIND text
IDX.QUERY posts MATCH "rust 全文检索" LIMIT 10 [FIELDS title body]
```

## Quick start (server)

Rows are hash keys under the declared prefix; the indexed value is
one declared field, exactly as with `range` indexes
([indexes.md](indexes.md)):

```console
kevy-cli -p 6004 IDX.CREATE posts ON PREFIX post: FIELD body TYPE str KIND text
kevy-cli -p 6004 HSET post:1 title "intro" body "kevy is a pure-Rust key-value store"
kevy-cli -p 6004 HSET post:2 title "search" body "dictionary-free CJK full-text search"
kevy-cli -p 6004 IDX.QUERY posts MATCH "full-text rust" LIMIT 10
```

The reply is a flat array of `key, score` pairs, best first. Add
`FIELDS` to hydrate named hash fields on each hit's owning shard in
the same call (no second round-trip):

```console
kevy-cli -p 6004 IDX.QUERY posts MATCH "search" LIMIT 10 FIELDS title
```

Supporting verbs work on text indexes like on every other kind:

- `IDX.EXPLAIN posts MATCH "full-text rust"` — the parse without the
  execution: kind, build state, estimated rows, and the plan line.
- `IDX.VERIFY posts` / `IDX.LIST` — entries / bytes / postings /
  token statistics, live.
- `IDX.DROP posts` — drop the declaration (catalog mutation,
  sidecar-persisted).

`MATCH` accepts `LIMIT` (≤ 1000) and `FIELDS`; there is no `CURSOR`
form (see "Matching and ranking" for why).

## Quick start (embedded)

Same engine in-process. The text kind is behind the `text` cargo
feature (on by default; `idx_create` with `IndexKind::Text` answers
`KevyError::Unsupported` when compiled out):

```rust
use kevy_embedded::{Config, IndexKind, IndexValType, Store};

fn main() -> kevy_embedded::KevyResult<()> {
    let store = Store::open(Config::default())?;

    store.idx_create(b"posts", b"post:", b"body", IndexValType::Str,
                     IndexKind::Text)?;   // builds synchronously
    store.hset(b"post:1", &[
        (b"title".as_slice(), b"intro".as_slice()),
        (b"body".as_slice(),
         b"kevy is a pure-Rust key-value store".as_slice()),
    ])?;

    // BM25-ranked hits, best first: Vec<(key, score)>.
    let hits = store.idx_match(b"posts", b"rust store", 10)?;
    for (key, score) in &hits {
        println!("{} {score}", String::from_utf8_lossy(key));
    }
    Ok(())
}
```

`idx_match` returns `KevyResult<Vec<(Vec<u8>, f64)>>`;
`KevyError::NotFound` names a missing index. There is no `FIELDS`
hydration embedded — you are in-process, read fields with `hget`.
Embedded builds are synchronous: `idx_create` returns when the index
serves (no `-INDEXBUILDING` phase to poll).

## Tokenization (dictionary-free)

- Latin/alphanumeric runs → lowercased word tokens (length ≥ 2).
- CJK (unified ideographs, kana, hangul) → **adjacent bigrams** —
  「全文检索」 indexes as 全文/文检/检索, so any two-character
  substring query matches without a dictionary. A lone CJK character
  emits itself.
- Tokens never cross a script boundary; queries tokenize with the
  same rules.

The bigram scheme is the whole CJK story: no dictionary to ship, no
dictionary to go stale, and mixed-script documents (`"Rust 入门"`)
index both halves under their own rules. The cost is recall noise on
one-character CJK queries (they match only lone-character emissions)
— query with two or more characters.

## Matching and ranking

`MATCH` is **OR semantics over query tokens, BM25-ranked** (k1=1.2,
b=0.75, non-negative idf variant). Documents matching more (and
rarer) query terms rank higher; term frequency saturates; long
documents are normalized down.

Two declared approximations:

- **Scores are shard-local.** df/avgdl come from each shard's own
  corpus (global statistics would require cross-shard write
  coordination). With hash-sharded keys the statistics converge
  across shards and the merged ranking is stable; scores from
  different shards are comparable, not identical to a single-corpus
  run.
- **No cursor.** BM25 deep pagination is an anti-pattern (page N
  requires re-scoring everything above it); `LIMIT` caps at 1000.

No phrase queries, no boolean syntax, no highlighting — that's the
query-engine slope (deliberately out of scope). If you need those,
you are describing a search engine; kevy's text kind stops at
"ranked lookup over declared fields".

## Hybrid retrieval (BM25 + KNN)

A text index and an ANN index over the same corpus fuse server-side
with reciprocal-rank fusion:

```
IDX.QUERY HYBRID posts MATCH "rust storage" embs KNN <f32-le-blob>
    [LIMIT n] [RRFK k] [EF ef] [FIELDS f…]
```

Each hit's fused score is `Σ 1/(k + rank_i)` over the two result
lists (`RRFK` default 60 — the standard fusion constant; rank
positions, not raw scores, so BM25 and distance need no calibration
against each other). See [vector-search.md](vector-search.md) for
the KNN half. Hybrid is a server verb; embedded callers run
`idx_match` + `idx_knn` and fuse in-process.

## Lifecycle and consistency

Same envelope as every index kind ([indexes.md](indexes.md)):

- A write and its segment update are **atomic within the owning
  shard** (single reactor thread / shard lock). An update removes
  exactly the old document's tokens and inserts the new ones —
  documents keep their original text for this, so there is no
  tombstone drift.
- A row whose declared field is missing simply contributes no
  tokens; there is no coercion failure for `TYPE str` text fields.
- Cross-shard queries merge per-shard top-K without a global
  snapshot (SCAN-class, same as `DBSIZE`).
- **Server backfill is asynchronous**: after `IDX.CREATE` on a live
  keyspace, or after a restart, queries answer `-INDEXBUILDING`
  until the rebuild completes (data availability never waits for
  index builds; poll or retry — see [indexes.md](indexes.md)).
  Embedded builds are synchronous.
- Inverted segments are **derived state** — never snapshotted or
  AOF-logged, rebuilt from data after restart.
- `MAXMEM` (on `IDX.CREATE`) caps the segment's memory; a build that
  crosses the budget fails declaratively (`-INDEXOVERBUDGET` on
  queries) instead of growing unbounded.

## Performance

Top-K evaluation uses MaxScore pruning (rarest terms first; commoner
lists are probed per candidate once they can't lift new documents
into the top K). Postings are doc-id inverted lists impact-ordered
two ways: tf buckets descending, sparse log2 dl bands ascending
inside each bucket — a single-term query (no second list to prune
against) stops exactly at the first band whose lower edge can't beat
the kth floor, so even the most common term answers in ~0.1ms at
200k docs (was ~6ms as a full postings scan). One-posting tokens
(the Zipf long tail) stay inline and cost no heap.

Measured envelope (receipts in the bench tree):

- [`bench/textgate.sh`](../bench/textgate.sh) gates `MATCH` p95
  < 20ms at 1M mixed-script documents (~100 bytes each) against a
  real server, plus the memory formula against real RSS growth.
  It runs in CI-adjacent release checks — the numbers are clamps,
  not aspirations.
- [`bench/PERF-LEDGER.md`](../bench/PERF-LEDGER.md) records the
  comparative shootout: BM25 top-10 at +21% qps with a p95 tie
  against RediSearch's `FT.SEARCH` on the same corpus.

The write side is the standard index tax: one hash-field read plus
one segment update per matching index per write; an empty catalog
costs one untaken branch per write.

## Sizing

`bytes ≈ Σ_token (token_len + 48) + postings × 64 + Σ_doc (key_len +
text_len + 72)` (docs keep their original text so updates remove
exactly their own tokens), reported live by `IDX.VERIFY` /
`IDX.LIST` (entries/bytes/postings/tokens for text kinds).
`bench/textgate.sh` gates the formula against real RSS growth.

Rebuild rides the standard backfill skeleton (tick-incremental on
the server, synchronous embedded).

## See also

- [indexes.md](indexes.md) — the index engine this kind plugs into
  (declaration grammar, cursor contract, consistency envelope).
- [vector-search.md](vector-search.md) — the ANN kind and the other
  half of `HYBRID`.
- [verb-reference.md](verb-reference.md) — generated grammar for
  every `IDX.*` form.
- [cookbook.md](cookbook.md) — full-text recipes in context.
