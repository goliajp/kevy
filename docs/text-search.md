# Full-text search (`KIND text`)

Text is the index engine's third kind: declare it like any index and
the field's raw bytes tokenize into a per-shard inverted segment,
maintained synchronously with every write (zero drift, same
derived-by-construction discipline as `range`/`unique`).

```
IDX.CREATE posts ON PREFIX post: FIELD body TYPE str KIND text
IDX.QUERY posts MATCH "rust 全文检索" LIMIT 10 [FIELDS title body]
```

Embedded: `idx_create(…, IndexKind::Text)` + `idx_match(name, query,
limit) -> Vec<(key, score)>`.

## Tokenization (dictionary-free)

- Latin/alphanumeric runs → lowercased word tokens (length ≥ 2).
- CJK (unified ideographs, kana, hangul) → **adjacent bigrams** —
  「全文检索」 indexes as 全文/文检/检索, so any two-character
  substring query matches without a dictionary. A lone CJK character
  emits itself.
- Tokens never cross a script boundary; queries tokenize with the
  same rules.

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
query-engine slope (REFUSED list).

Top-K evaluation uses MaxScore pruning (rarest terms first; commoner
lists are probed per candidate once they can't lift new documents
into the top K). Postings are impact-ordered two ways (v3.5): tf
buckets descending, and dl groups ascending inside each bucket — a
single-term query (no second list to prune against) stops exactly at
the first dl group that can't beat the kth floor, so even the most
common term answers in ~0.1ms at 200k docs (was ~6ms as a full
postings scan).

## Sizing

`bytes ≈ Σ_token (token_len + 48) + postings × 64 + Σ_doc (key_len +
text_len + 72)` (docs keep their original text so updates remove
exactly their own tokens),
reported live by `IDX.VERIFY` / `IDX.LIST` (entries/bytes/postings/
tokens for text kinds). `bench/textgate.sh` gates MATCH p95 < 20ms @
1M mixed-script docs and the formula against real RSS growth.
Rebuild rides the standard backfill skeleton (tick-incremental on the
server, synchronous embedded); inverted segments are derived state —
never persisted, rebuilt after restart.
