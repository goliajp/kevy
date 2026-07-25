# RFC — the terminal text query surface

First step of the FTS arc (`.claude/notes/fts-arc-design-round.md` §4,
decisions recorded in `.claude/scope-decisions.md`).

**This step is pure protocol design. No index structures, no door
churn.** It exists because v4 is the release that breaks API once and
then freezes it, and every capability the arc will add — phrase, filter,
facet, highlight, typo, prefix, sort, distinct — wants to change the same
signature. Designing them in one shape now costs a document; discovering
them one at a time costs a breaking change each.

## Today

```
IDX.QUERY <name> MATCH <text> [LIMIT n] [FIELDS f…]
```

Reply: a flat list of `(key, score)`. `FIELDS` is *hydration* — extra
fields echoed back — not field-scoped search.

Three things about this shape do not survive the arc:

1. **`<text>` is one opaque string.** Phrase, boolean and field-scoped
   queries all need it to have structure.
2. **The reply is positional `(key, score)`.** Highlighting adds spans,
   faceting adds counts, and both would have to change the tuple.
3. **There is nowhere to put a filter.** A filter is not a ranking
   concern and cannot be smuggled into the match text.

## The shape

```
IDX.QUERY <name> MATCH <text>
    [IN <field> [<field> …]]        field scope
    [FILTER <expr>]                 non-scoring predicate
    [FACET <field> [<field> …]]     counts per value
    [SORT <field> ASC|DESC]         override score order
    [DISTINCT <field>]              collapse by value
    [HIGHLIGHT [<field> …]]         spans in the reply
    [TYPO 0|1|2|AUTO]               edit distance budget
    [LIMIT n] [OFFSET m]
    [FIELDS f…]                     hydration (unchanged)
```

Every clause after `MATCH <text>` is optional and order-independent.
Adding a capability later adds a keyword; it never changes an existing
one. That is the whole point of doing this now.

### `<text>` gets a grammar, and it is the smallest one that works

```
term            bare word
"a b c"         phrase
+term           required
-term           excluded
field:term      field-scoped (sugar for a one-term IN)
```

No parentheses, no `AND`/`OR`/`NOT` keywords, no nesting. This is
deliberately the Meilisearch/Lucene-lite subset rather than a boolean
algebra: the moment nesting is allowed, the query language needs
precedence rules, an AST on the wire, and an optimiser — that is a
query engine, and the [scope reversal](../scope-decisions.md) walked
down the search-engine slope, not the database one.

`FILTER` is where structured predicates go, and it reuses the existing
index expression grammar rather than inventing a second one.

### The reply becomes a map, once

RESP3 map (RESP2: flat array of the same pairs, per the existing
convention):

```
hits    → [ {key, score, [highlights]}, … ]
total   → integer (or an estimate flag, see below)
facets  → {field → {value → count}}    (only when FACET was asked)
```

Moving to a named reply is the breaking part, and it is why this belongs
in v4 rather than after it. Once it is a map, `highlights` and `facets`
appear only when requested and nothing that exists today has to change
shape again.

**`total` is a commitment, so state it precisely.** With MaxScore
pruning the engine does not visit every matching document — that is why
it is fast. An exact total would defeat the pruning. So `total` is
exact when the result set fits within the scanned window and flagged
`estimated` otherwise, and the documentation says so. A number that is
silently approximate is the kind of thing this codebase has already been
burned by once, in a durability guarantee that quietly depended on
transaction size.

## What this RFC does NOT decide

- **Positional index** and **ordered term dictionary** — the two
  structures the arc is actually about. This surface is designed so they
  can land behind it without another wire change, which is the point,
  but their design is separate.
- **Ranking rules.** Meilisearch exposes an ordered rule list
  (typo, words, proximity, attribute, exactness). Whether kevy exposes
  the same knob or fixes a single ranking is a product decision that
  should be made when there is something to rank with.
- **Per-attribute weights.** Needs the multi-field `IndexSpec` from
  decision 1 to exist first.

## Order of construction

1. **This surface**, parsed and validated, with everything past
   `MATCH <text>` accepted and the unimplemented clauses returning a
   clear "not yet" error rather than being silently ignored. A clause
   that parses and does nothing is worse than one that is refused.
2. **Multi-field `IndexSpec`** (decision 1) + sidecar v2 — `IN`,
   `field:term` and per-attribute weighting all need it.
3. **Global BM25 statistics snapshot** (decision 2), with the staleness
   window documented rather than glossed.
4. **Positional index** → phrase, proximity, `HIGHLIGHT`.
5. **Ordered term dictionary** → prefix, `TYPO`.
6. `FILTER`, `FACET`, `SORT`, `DISTINCT` — each is a layer over
   structures that exist by then.

`bench/textgate.sh`'s memory clamp is re-baselined **inside** steps 4
and 5, not after: both change the memory formula, and a gate re-based
after the fact records whatever happened rather than checking it.

## Why step 1 first, concretely

It is the only step that is purely additive to the wire and purely
subtractive to future risk. It also front-loads the door question from
the design round §5: Swift, JNI and N-API have no typed IDX surface at
all and reach it through `cmd()`. If the terminal shape is fixed now,
those doors can be given a typed surface once, against a signature that
will not move — instead of being given one per capability.
