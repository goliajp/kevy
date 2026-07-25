# RFC — the FTS deep structures (global BM25, positions, term dictionary)

Phase A for the three steps the FTS arc's design round named as the real
work (`.claude/notes/fts-arc-design-round.md` §2: "scope the arc around
the two structures, not the checklist"). The terminal query surface
(step 1) and multi-attribute documents (steps 2–3) have shipped; these
are what remains, and they are different in kind.

**Status: design, not implementation. Read the verification boundary in
§0 before treating any of this as ready to build.**

## 0. Why these are not CI-only steps

Everything shipped so far was verifiable end-to-end in CI: parser
behaviour, sidecar round-trips, weighted scoring on small corpora. These
three are not, and pretending otherwise is the trap.

Each **changes the memory formula** `bench/textgate.sh` clamps, and
textgate needs a real server and ~1M documents — it is not a CI job, it
runs on lx64. So each step splits into two halves with different homes:

- **Correctness** — CI. Global stats produce the right ranking; a phrase
  query returns only documents with the adjacent terms; a prefix query
  returns the right key set. Unit-testable on tiny corpora.
- **Cost** — lx64 textgate. The memory formula still explains real RSS
  growth (0.5×–1.5×), and query p95 stays under budget. **A step does
  not merge until its textgate baseline is re-recorded and green.**

The µs/byte figures below are **estimates from structure, not
measurements** — per `perf-decomposition-vs-polish.md`, Phase A estimates
are hand-waves until lx64 rules. They exist to size the work and to say
which structure to reach for, not to claim a result.

This is also why they should not be built at the tail of a long session:
a stone-layer core-structure change whose verifier lives on another box
is exactly the kind of work that wants a fresh attention cycle, not the
end of one.

## 1. Step 4 — global BM25 statistics (scope decision 2)

### The gap

`bm25(tf, df, n_docs, dl, avgdl)` has three corpus-level terms, and
today all three are **shard-local** (`segment.rs`: `n_docs =
self.docs.len()`, `avgdl = self.total_len / n_docs`, `df =` this shard's
posting-list length). Scores from different shards are comparable but
not identical to a single-corpus BM25. Meilisearch's ranking assumes
global statistics.

### The move, and the cost that makes it non-trivial

`n_docs` and `avgdl` aggregate cheaply: two numbers per shard, summed.
**`df` does not.** It is per-token, and a real corpus has hundreds of
thousands of tokens — a global df is the union of every shard's
token→df, rebuilt periodically. That union is the memory cost, and it is
why `segment.rs:3` records the original decision as "global stats need
cross-shard write coordination".

Scope decision 2's ruling holds: **do not touch the write path.**
Aggregate a global-stats snapshot periodically, off the write path, and
score against it.

### Design

- A `GlobalStats { n_docs: u64, total_len: u64, df: HashMap<token, u32> }`
  behind an `ArcSwap`-style pointer (a plain `Arc<GlobalStats>` swapped
  under a mutex — no third-party dep), rebuilt on a timer by walking each
  shard's segment under a read lock and summing.
- `matches()` reads the current snapshot instead of `self.docs.len()` /
  local df. The MaxScore upper bound `bm25_upper` uses the same global
  numbers, so pruning stays correct (global N ≥ local df always, so the
  idf term is well-defined).
- **Staleness is a documented number, not a footnote.** df moves slowly
  on a real corpus, so an N-second-old snapshot shifts rankings within
  noise — but the docs must say "global stats refresh every N seconds"
  and must not claim "identical to single-corpus BM25". This is the
  obligation scope decision 2 already wrote down, and the same discipline
  as the three competitive ratios corrected down this cycle: never claim
  a precision the mechanism does not have.

### Phase A finding — a query-time variant that beats the snapshot

Reading `idx_match` (`ops_index.rs:239`) changed the plan. It already
fans out across shards — each shard runs `ts.matches` on its own segment
and the origin merges. The scores are incomparable precisely because
each `matches` uses its shard's local stats.

The snapshot design carries a global **df table** for the whole corpus
because it does not know the query in advance. But at query time it
does, and **a query has a handful of tokens** — so the only df values
that matter are those few, not the hundreds of thousands in the corpus.

That admits a **two-pass, query-time** aggregation with no snapshot at
all:

1. Pass 1: each shard reports its local `(n_docs, total_len, df[t] for
   each query token t)` — a few integers, not a table.
2. Origin sums them into global `(n_docs, avgdl, df per query token)`.
3. Pass 2: each shard runs `matches` scored against those global
   numbers.

This is **strictly better than the snapshot on correctness**: no
staleness window at all, scores exact as of the query. It carries **no
steady-state df-table memory**. It still never touches the write path.
Its cost is one extra light fan-out per cross-shard text query — a few
integers per shard — which is what lx64 has to weigh against the
snapshot's single pass plus staleness.

**This reverses part of scope decision 2** (which chose a periodic
global snapshot). The reversal is on evidence — the snapshot assumed a
whole-corpus df was unavoidable, and the query already narrows it — so
scope-decisions gets updated, not silently overwritten, once lx64
confirms the two-pass latency is acceptable. Until then both are live.

### The split that is CI-doable now (step 4a)

Both designs share one core: `TextSegment::matches` must be able to
score against **externally supplied** corpus stats instead of its local
ones. That is a stone-layer capability with a pure-correctness test —
two segments each holding half the documents, scored with the summed
global stats, rank identically to one segment holding all of them — and
it does not change the default (no-stats) path at all, so it needs no
perf gate. **Step 4a is that capability; step 4b is the cross-shard
aggregation (two-pass vs snapshot) that lx64 rules on.**

### Verification split

- **CI (4a)**: a segment scored with injected global stats matches a
  single-corpus reference; the no-stats path is byte-identical.
- **lx64 (4b)**: two-pass fan-out latency vs snapshot+staleness at 1M
  docs; whichever wins, its cost against the memory/latency formula.

### Estimated cost (lx64 to confirm)

Global df table ≈ Σ_distinct_token (token_len + 8) across the corpus —
one entry per token that appears anywhere. For a 1M-doc English corpus
(~200k distinct tokens) ≈ 3–4 MB per store, one copy, rebuilt on the
timer. Small next to the postings, but it is new steady-state memory the
formula must account for.

## 2. Step 5 — positional index (phrase, proximity, highlight)

### The gap

Postings store tf, not positions. `"quick brown"` as a phrase, `NEAR`,
and highlight spans all need to know **where** in the document each term
occurred. This is the larger of the two missing structures.

### Design

- Each posting gains a positions list: token → key → **(tf, [u32
  positions])**. Positions are the token offsets within the document's
  concatenated fields, delta-encoded (varint) since they are ascending —
  the standard Lucene layout, and the delta+varint keeps a
  high-frequency term from paying 4 bytes per occurrence.
- Phrase query: intersect the candidate set (the current tf path), then
  verify adjacency by walking the two position lists in lockstep. This
  is `O(matching docs × positions)`, gated behind the same MaxScore
  candidate pruning so a phrase over common terms does not walk the
  whole list.
- Highlight: the position list is already what a snippet generator
  needs; `HIGHLIGHT` returns spans, which is why the terminal surface
  (step 1) reserved a `highlights` slot in the reply map rather than a
  flat `(key, score)` tuple.
- **This is the memory-heavy step.** Positions can multiply posting
  bytes several-fold for long documents, so it is feature-gated and the
  textgate memory formula changes materially. Storing positions is a
  create-time choice on the index (`WITH POSITIONS`), because a corpus
  that never runs a phrase query should not pay for them.

### Verification split

- **CI**: `"quick brown"` matches a document with the words adjacent and
  not one with both words far apart; a proximity query respects the
  window; highlight spans point at the right offsets. All tiny-corpus.
- **lx64**: positions memory against the revised formula; phrase-query
  p95 at 1M docs; the delta+varint encoding actually holds the
  per-posting growth where the estimate says.

### Estimated cost (lx64 to confirm)

Adds ≈ Σ_posting (dl_of_that_posting × ~1.3 bytes varint-delta) — for
100-token documents, roughly doubles posting bytes. This is the estimate
most likely to be wrong (varint compression depends on term density),
which is exactly why it is feature-gated and lx64-verified before it can
be a default.

## 3. Step 6 — ordered term dictionary (prefix, typo)

### The gap

`postings` is a `HashMap`, so there is no way to enumerate terms by
prefix. Prefix / search-as-you-type and Levenshtein-automaton typo
tolerance both need the terms in order.

### Design

- The term dictionary moves from `HashMap<token, Buckets>` to an ordered
  structure — a sorted `Vec<(token, Buckets)>` with binary search is the
  minimum, an FST (finite-state transducer, the Lucene/Tantivy choice)
  the eventual one. **Start with the sorted vec**: it unlocks prefix
  immediately, its cost is measurable, and an FST is a later compression
  of the same interface rather than a prerequisite.
- Prefix query: binary-search the lower bound, walk while the prefix
  matches, union the posting lists. `TYPO n` (from the terminal surface)
  builds a Levenshtein automaton and intersects it with the ordered
  dictionary — but that is a step past this one; step 6 is the ordered
  substrate that makes it possible.
- **The risk here is in the write path, not memory.** Every insert must
  keep the dictionary ordered, so `apply` moves from O(1) hash insert to
  O(log n) find + possible shift. This is the one deep step that touches
  the write path, so its perf gate is write throughput, not just query
  latency — and scope decision 2's "do not touch the write path" is
  about the BM25 stats, not this; here the write cost is the deliberate
  price of prefix, and it must be measured, not assumed tolerable.

### Verification split

- **CI**: a prefix returns exactly the terms sharing it and no others;
  the dictionary stays ordered across inserts, updates and deletes
  (a property test over random operations).
- **lx64**: write-throughput delta vs the HashMap baseline at 1M docs;
  prefix-query p95; whether the sorted-vec shift cost forces the FST
  sooner than planned.

## 4. Also pending, but shallow — step 3.5, the CREATE wire syntax

Not a deep structure, listed here so it is not lost: multi-field indexes
are creatable through the embedded API but `IDX.CREATE` still parses one
`FIELD`. A `FIELDS a b [WEIGHTS w…]` clause is **purely additive** —
existing `FIELD f` is byte-unchanged — so it does not freeze with 4.0
and is not on this RFC's critical path.

It is a steel-tier parser change, not a stone one: today's positional
parser (`FIELD` at a fixed offset, `TYPE` at the next) becomes a scan to
the `TYPE` keyword when the verb is `FIELDS`. Fully CI-verifiable, no
perf gate. The one caution is that the single-`FIELD` path must stay
byte-identical — every existing index test depends on it — so the two
verbs get two parse paths rather than one merged scanner.

## 5. Order, and the gate between each step

1. **Step 3.5** (CREATE syntax) — shallow, CI-only, do whenever.
2. **Step 4** (global BM25) — smallest deep step, least write-path risk.
3. **Step 5** (positions) — largest memory change; feature-gated.
4. **Step 6** (ordered dictionary) — only write-path change.

Between each: `bench/textgate.sh` re-baselined **inside** the step that
changes the formula, on lx64, green before merge. A gate re-based after
the fact records what happened instead of checking it — the same reason
covgate's baseline is a ratchet, not a post-hoc snapshot.

Steps 4–6 are stone-layer core-structure changes to `kevy-text`, the
crate with the deepest review requirement and the widest bug-blast
radius (`steel-cement-stone`: quality effort scales with blast radius).
None should be built without its lx64 verifier in reach.
