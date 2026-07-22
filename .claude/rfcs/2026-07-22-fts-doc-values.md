# Doc values — what `FILTER`, `SORT`, `DISTINCT` and `FACET` actually need

Addendum to [the terminal query surface](2026-07-21-fts-terminal-query-surface.md)
and [the deep structures](2026-07-21-fts-deep-structures.md), written after
steps 4–7 landed (global BM25, positions, ordered-dictionary prefix/typo,
per-field postings).

## The assumption this refutes

The surface RFC's build order ends:

> 6. `FILTER`, `FACET`, `SORT`, `DISTINCT` — each is a layer over
>    structures that exist by then.

That is not true, and it is worth saying plainly rather than discovering
it one clause at a time.

Everything built so far maps **term → documents**: the impact-bucketed
postings, the positional side-channel, the per-field breakdown. Every one
of the four remaining clauses needs the opposite direction — **document →
its own value**:

| clause | what it must read per document |
|---|---|
| `FILTER price RANGE 10 100` | that document's `price` |
| `SORT published DESC` | that document's `published` |
| `DISTINCT author` | that document's `author` |
| `FACET category` | that document's `category`, for every match |

An inverted index cannot answer any of those without walking every
posting list of every possible value. This is the same gap Lucene closed
with **doc values**, and for the same reason. So the four clauses are not
four layers over what exists; they are **one missing structure** plus four
thin layers over it.

That is good news for the build order: one structure unlocks all four.

## Decision

Store, per document, the raw bytes of the fields an index declares as
filterable — a columnar side-channel beside the postings, in the shape
[`positions`](../../crates/kevy-text/src/positions.rs) and
[`fields`](../../crates/kevy-text/src/fields.rs) already established:
`Option<DocValues>`, present only when the index declares some, never
touched by a query that does not use one.

Declared at CREATE, not inferred:

```
IDX.CREATE idx ON PREFIX p: FIELDS title body TYPE str KIND text
    [WITH POSITIONS] [VALUES price status category]
```

`VALUES` names hash fields the index will store per document. A clause
naming a field that was not declared is an **error naming what was**, the
same contract `IN` established — a filter that silently matches nothing
is indistinguishable from a corpus with no hits.

### Why not read the row per candidate

The obvious alternative needs no declaration and no storage: for each
candidate document, read the field off its hash row. It is genuinely more
convenient, and it is what a database would do.

It is rejected on the project's stated mandate — **上限性能优先**. A
broad query has candidates in the hundreds of thousands; a hash lookup
each turns a scoring walk into a scan of the corpus. Worse, the cost is
paid on the *broad* queries, which are exactly the ones already closest to
their latency ceiling. Doc values make the same test an O(1) indexed read.

It also does not generalise: `FACET` must count values across the whole
match set, and `SORT` must order by a key the postings are not ordered by.
Per-candidate row reads make both quadratic in the wrong dimension.

### Why not intersect a companion index

The second alternative reuses what exists: require a range index on the
filtered field, resolve the predicate to a key set, intersect.

Rejected for two reasons. A predicate can be broad — `RANGE 0 1000000`
resolves to a set the size of the corpus, and materialising it per query
costs more than the search. And it does not reach `SORT` or `FACET` at
all: an index tells you which keys qualify, not what value each hit
*has*, which is the number those clauses report.

Doc values subsume it: with the value in hand, a predicate is a
comparison, an order is a sort key, and a facet is a count.

## Typing stays out of kevy-text

`kevy-text` stores raw bytes and knows nothing about numbers, dates or
collation. A predicate arrives as a test over those bytes:

```rust
pub struct Filter<'a> {
    /// Which declared value field the predicate reads.
    pub field: usize,
    /// The test applied to that field's raw bytes. A document with no
    /// value for the field never passes — absent is not a value.
    pub test: &'a dyn Fn(&[u8]) -> bool,
}
```

The caller owns typing, and it already has the vocabulary for it:
`kevy-index`'s `ValType` / `IndexValue` coercion is the grammar the
surface RFC meant by "reuses the existing index expression grammar". So
`FILTER price RANGE 10 100` parses with the range-query parser, coerces
with the index coercion, and reaches the segment as a closure. One
grammar, one coercion, and the text stone stays a stone.

## Where the predicate applies

**Before the top-K, never after.** Scoring ten hits and then dropping
eight returns two — the seven qualifying documents ranked 11th onward are
simply lost. Over-fetching hides it until the filter is selective enough,
which is the worst way for it to fail.

Concretely the test goes in `select_top`, where the candidate set is
walked exactly once: every candidate is tested at most once (cheaper than
testing inside each term's accumulation, which retests a document per
query term), and only survivors enter the heap.

MaxScore pruning stays correct under this. Pruning drops lists whose best
possible contribution cannot beat the k-th score so far; filtering keeps
that k-th score *lower*, so it prunes less, never more. Slower on a
selective filter, never wrong.

## Build order

1. **8.a stone** — `DocValues` in `kevy-text`: declared arity, `id →
   value` flat storage, maintained by `apply_fields`/`withdraw` like the
   other channels, plus its term in the memory formula. `QueryOpts` gains
   `filter: &[Filter]` (ANDed) applied in `select_top`.
2. **8.b declaration** — `VALUES f…` on `IDX.CREATE`, `IndexSpec`, the
   sidecar (v3 → v4, the same bump `WITH POSITIONS` made), and the
   indexing path in **both** crates that build segments — the server's
   `index_runtime` and the embedded `ops_index_sync`, which step 7 caught
   drifting apart.
3. **8.c FILTER** — parse with the range-query grammar, coerce with the
   index coercion, thread through both passes like every other clause.
4. **8.d gate** — `textgate VALUES=1`: the memory term calibrated against
   real RSS (a new term is calibrated, not asserted — step 5f's first
   model explained 0.42× of it) and a filtered-query p95 ceiling.
5. **SORT / DISTINCT / FACET** — each now a thin layer, but each with its
   own distributed question, which this addendum does not decide:
   - `SORT` and `DISTINCT` are top-K **by a different key**. Shards rank
     by BM25 and return their best; re-ordering or collapsing that is not
     a global answer. Either the shard fetch deepens (approximate, and
     say so) or the fan-out gains a round.
   - `FACET` counts over the **whole** match set, not the top-K, so it is
     a different reduce shape rather than a different sort.

Doing 1–4 first is deliberate: `FILTER` is the one of the four whose
distributed semantics are already settled (a predicate commutes with the
merge — a document either qualifies on its own shard or it does not), so
it validates the structure without also opening the top-K question.

---

# Addendum 2 — the remaining three are exact, not approximate

Written after 8.a–8.d landed. It corrects this document's own step 5,
which said `SORT` and `DISTINCT` were "top-K by a different key… either
the shard fetch deepens (approximate, and say so) or the fan-out gains a
round." Both alternatives were unnecessary. All three clauses have exact
cross-shard answers with one round, and the mistake was assuming a shard
must rank by score.

## Why the fan-out is exact for score

A shard returns its best `limit + offset` by BM25 and the origin merges.
That is exact because scores are comparable across shards (pass 1 injects
global statistics) and the global top-K of a union of sorted lists is
contained in the union of their top-Ks.

Nothing in that argument mentions *score*. It holds for **any** total
order the shards agree on.

## SORT — rank by the sort key, not by score

The error was to picture shards ranking by BM25 and the origin re-sorting.
Of course that loses documents: a late-`published` row can sit 50th by
score and never be sent.

So do not rank by score. `SORT published DESC` makes the shards select
their top `limit + offset` **by `published`**, over all their matches, and
the origin merges by `published`. The k-way merge argument applies
verbatim, so the answer is exact. Scores are still computed and reported —
they are just not what the selection orders by.

Concretely: the top-K selection takes a comparator. Doc values supply the
key per document, and the walk that already visits every candidate exactly
once does the selection with a different comparison.

## DISTINCT — collapse per shard, then again at the origin

Each shard collapses its matches by the key (best-scoring document per
value) and returns its top `limit + offset` collapsed; the origin
collapses across shards and takes the top K.

This is exact, and the proof is short. Take any value in the true global
top-K, represented by its best-scoring document `d`, on shard `S`.
Suppose `d` were not in `S`'s local top-K-collapsed: then K distinct
values beat it on `S`, and each of those values' *global* best is at
least its score on `S`, hence also beats `d`. That is K distinct values
above `d` globally — so `d` was not in the global top-K after all.
Contradiction.

## FACET — count before truncating, sum at the origin

Facet counts are over the whole match set, not the top-K, so the shard
must count before it truncates. It already has the full candidate set in
hand (accumulation produces every match before selection), so counting is
a pass over what is already there, and the origin sums the per-value maps.
Exact.

The one real cost is that a per-shard map is as large as the field's
cardinality within that shard. That is a cost, not an inexactness, and it
is bounded by what the index declared: `FACET` reads a declared `VALUES`
field, opt-in per query. Truncating per shard — Elasticsearch's
`shard_size`, with its `doc_count_error_upper_bound` — buys a smaller
chunk at the price of counts that can be wrong in the tail. Exact first;
if a real corpus makes the chunk hurt, the approximation can be added
*and declared* rather than assumed now.

## What this changes about the build order

Nothing about the structure — doc values remain what all three read.
What changes is that none of them needs a second round or an accuracy
disclaimer. The work is: teach the top-K selection to order by a
comparator (SORT), to collapse by a key (DISTINCT), and teach the reduce
to sum per-value maps (FACET).

---

# Addendum 3 — FACET's three decisions

Written before implementing the last clause. Each of these is a choice
with a defensible alternative, so they are recorded rather than left in
the code to be inferred.

## Where the counts go in the reply

The surface RFC planned for the reply to become a map (`hits` / `total` /
`facets`). It has not, and rows are still flat arrays — `HIGHLIGHT`
landed as an additive trailing element per row precisely so the row shape
would be unchanged when the clause is absent.

`FACET` is per query, not per row, so it takes the same treatment one
level up: when the clause is present the top-level array gains **one
final element**, `[field, [value, count, …], field, …]`. A query without
`FACET` is byte-identical to before.

Restructuring the whole reply into a map for one clause would change
every client's parse for a feature most of them do not use. If the map
shape is wanted it should be a deliberate surface migration, not a side
effect of FACET.

## Counted over the matches, not the page — and not the collapsed set

Facets describe **what matched**, so they are counted before the top-K
truncation. That is the whole reason they need doc values: the page is
`limit` documents and the answer is about all of them.

`FILTER` does restrict the count, because a filtered-out document did not
match. `DISTINCT` does **not**: collapsing decides which documents are
shown, not which matched, and "how many matching documents per category"
is the question a facet answers. Same split Elasticsearch draws between
`collapse` and aggregations.

## Bucket identity is the coerced value; the label is a stored spelling

Two shards can hold `1` and `1.0` in a field declared `f64`. Those are
one value, so they must be one bucket — the same identity `DISTINCT`
groups by, which is `order_key`.

But a bucket has to be *reported*, and the coerced identity is an opaque
encoding. So a shard sends `(key, label, count)` and the origin sums by
`key`, reporting the first label it saw for it. The label is therefore
always a spelling that really occurs in the corpus, never a
re-serialisation invented by the engine.

## Cost, stated

A shard's map is as large as the field's cardinality within that shard.
That is a cost, not an inexactness. Truncating per shard (Elasticsearch's
`shard_size`, with its `doc_count_error_upper_bound`) would trade exact
counts for a smaller chunk; if a real corpus makes the chunk hurt, that
approximation can be added **and declared**, not assumed now.
