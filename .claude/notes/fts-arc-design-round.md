# FTS arc — design round (not a plan to execute yet)

**Ask:** strengthen full-text search in v4 until kevy can fully replace
Meilisearch, and beat it on perf / disk / mem.

**Status:** design round only. Two structural questions below need a
ruling before a query surface can be drawn, and nothing should be built
until they are answered. This document exists so that ruling is made on
evidence rather than on a feature list.

---

## 0. This reverses a locked scope decision

`docs/text-search.md:118` says, in the shipped documentation:

> No phrase queries, no boolean syntax, no highlighting — that's the
> query-engine slope (deliberately out of scope). If you need those,
> you are describing a search engine; kevy's text kind stops at
> "ranked lookup over declared fields".

The ask is precisely to walk down that slope. That is a legitimate change
of direction, but it is a **reversal, not an extension**, and it should be
recorded in `.claude/scope-decisions.md` as one — with the new reasoning —
rather than silently overwritten. Everything below assumes the reversal is
approved.

---

## 1. What exists today (measured, not assumed)

`crates/kevy-text`, **951 LOC**, zero third-party deps.

| File | LOC | Role |
|---|---|---|
| `segment.rs` | 404 | index apply, MaxScore query, stats |
| `buckets.rs` | 204 | impact-bucketed posting lists |
| `segment_tests.rs` | 155 | tests |
| `token.rs` | 129 | the entire analyzer |
| `bm25.rs` | 44 | scoring + MaxScore bound |

**The honest read: this is a well-built *scoring engine*, not a search
engine.** MaxScore/WAND-family pruning, posting lists impact-ordered twice
(tf desc, then log2(dl) asc), hapax legomena kept inline with zero heap
after textgate caught a +2GiB RSS shape. Against RediSearch on a 200k Zipf
corpus it is p95-tied and +21% qps (`bench/PERF-LEDGER.md:112`). **None of
this needs replacing.** The arc adds structures around it.

## 2. The gap is two missing index structures, not a feature list

Grepping the tree for `levenshtein|typo|synonym|stopword|stemm|highlight|
snippet|facet|distinct|sortable|filterable|prefix.search|federat` returns
**zero implementation hits**. But nearly every missing feature traces back
to one of two absences:

| Missing structure | What it blocks |
|---|---|
| **Positional index** (only tf is stored) | phrase, proximity, highlighting/snippets |
| **Ordered term dictionary / FST** (postings is a `HashMap`) | prefix, search-as-you-type, and the natural substrate for Levenshtein-automaton typo tolerance |

Everything else — synonyms, stop words, stemming, ranking rules — is a
layer over the analyzer or the scorer and is comparatively cheap. **Scope
the arc around the two structures, not around the checklist.**

Both change the memory formula and therefore `bench/textgate.sh`'s
memory-honesty clamp (`IDX.VERIFY bytes / RSS growth` in 0.5–1.5×). The
gate is re-baselined **as part of** the arc, not after it.

## 3. Two structural questions that need a ruling

These are architectural commitments with recorded rationale. They are not
implementation details and I should not decide them alone.

### Q1 — one-index-one-field, or multi-attribute documents?

Today an index is **one key prefix + one hash field** (`IndexSpec`). There
is no multi-field document model and no per-field weighting; `FIELDS f…`
on a query is *hydration* (extra fields in the reply), not field-scoped
search.

Meilisearch's model is multi-attribute documents with per-attribute
ranking and attribute-level filter/sort/facet declarations. There is no
path from the current model to that without changing `IndexSpec`.

- **Keep single-field**: cheap, preserves the current write path, but
  "fully replace Meilisearch" is then false for any multi-attribute
  corpus — which is most of them.
- **Go multi-attribute**: honest replacement, but it changes the index
  spec, the write hook, the catalog sidecar format, and the query surface
  all at once.

### Q2 — shard-local BM25 statistics, or global?

`segment.rs:3-5` records the decision: df/avgdl are per-shard because
global statistics would need cross-shard write coordination.
`docs/text-search.md:113` documents the consequence — scores from
different shards are comparable, **not identical to a single-corpus BM25**.

Meilisearch's ranking-rule semantics assume global statistics. Relevance
that is "approximate by construction" is defensible for ranked lookup and
is harder to defend as a Meilisearch replacement.

- **Keep shard-local**: no write-path cost, relevance stays approximate.
- **Go global**: needs a cross-shard df/avgdl aggregation with a staleness
  budget, and a decision about what happens to scores while it is stale.

**My recommendation on both: go multi-attribute (Q1), keep shard-local
with a periodically-synced global df estimate (Q2).** Q1 has no
workaround; Q2 does — a periodically refreshed global df is
within-noise for ranking at realistic corpus sizes and avoids putting
coordination on the write path. But both are the user's call.

## 4. Query surface: design the end state once

Today: `IDX.QUERY <name> MATCH <text> [LIMIT n] [FIELDS f…]`.

The trap is growing this incrementally. Every capability below wants to
change the shape:

| Capability | What it needs on the wire |
|---|---|
| phrase / boolean / field-scoped | `text` stops being one opaque string |
| filter + facet | a filter clause, plus facet counts in the reply |
| highlight | reply is no longer `(key, score)` but carries spans |
| typo / prefix | query options |
| sortable / distinct | more options |

**Design the terminal surface first, so later capabilities add only
optional blocks and never change an existing signature.** This step is
pure protocol design — zero door churn.

## 5. Door impact — the part that is easy to underestimate

Measured across the bindings:

| Door | text surface today |
|---|---|
| Go | **typed**: `IdxQueryMatch(name, text, limit) -> []Ranked`, plus `IdxQueryRaw` |
| C#, Python, Flutter, Nitro, Expo | have an IDX surface |
| Swift, JNI, N-API | **none** — they reach IDX through the generic `cmd()` channel only |

That Go signature — `(name, text, limit) -> [](key, score)` — freezes
today's capability into every typed door. Every item in the table above
either adds a parameter, adds an overload, or replaces it. Times the doors
that have a typed surface.

**Proposed staging:**

1. **Protocol first.** Terminal query surface designed and shipped
   server-side. Zero door changes.
2. **Doors expose raw + one opaque query builder** during the capability
   build-out — not a typed method per capability, or every iteration is a
   9-door change. Swift/JNI/N-API are the existing proof that `cmd()` is
   enough to be useful.
3. **Typed surfaces land once, after capabilities freeze**, verified by
   ffigate in a single pass.

## 6. Persistence — the disk axis needs a decision too

**The text index is not persisted.** Only the catalog (index definitions)
is, as a tab-separated sidecar; on boot every index loads in `Building`
state and backfills from the source hashes. PERF-LEDGER records 47.3s to
build four indexes over 200k docs.

Comparing disk against Meilisearch is meaningless until this is settled:
Meilisearch persists its index. Either kevy persists the postings (new
format, new recovery path, new crashgate cells) or the disk comparison is
explicitly "kevy stores only the source data" — which is a legitimate and
much better disk story, but must be stated as such rather than measured
as if the two were doing the same work.

## 7. Measuring: `meiligate`, and the ruler comes first

The project corrected three published competitive ratios downward today
because the old measurements read `redis-benchmark`'s self-reported rate,
which is quantized low and unevenly per engine. Do not repeat that.

- **Corpus: Meilisearch's own public benchmark datasets** (hackernews,
  movies). Not a self-made corpus — a self-made corpus proves nothing to
  anyone outside this repo.
- **Both engines measured the same way**, server-side counters where
  available, median-of-N with per-cell stdev, and a gap smaller than the
  stdev reported as NOISE — the `arena.sh` discipline.
- **Three axes, separately**: query latency/qps, index build time, RSS and
  on-disk bytes at rest. A win on one is not a win.
- **Recall/relevance must be aligned before speed is compared.** A faster
  engine that returns worse results is not faster. Meilisearch's default
  ranking-rule chain includes typo tolerance; comparing kevy's exact-match
  BM25 against it without aligning the retrieval quality is the same
  category error as the quantized ruler.

## 8. Layering (steel/cement/stone)

- **Stone**: analyzer (stemming, stop words, Unicode normalization,
  CJK segmentation), positional posting lists, term dictionary / FST,
  Levenshtein automaton, scorer. All business-free, all independently
  benchable and fuzzable.
- **Steel**: index orchestration, query planning, the catalog, the
  cross-shard merge.
- **Cement**: the verb handlers and reply encoders.

Build order follows the methodology: grow the stones first with their own
benches and fuzz targets, then the steel, then the verbs — so that by the
time `meiligate` runs end to end, most defects are already dead.
