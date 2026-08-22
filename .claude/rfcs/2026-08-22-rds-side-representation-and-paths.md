# RFC — the RDS side: what a declared table costs, and what the declaration could have paid for

Owner directive (2026-08-22): after the concurrency axis lands, "通过架构
设计看有没有办法在内存上和其他不足的面尽最大的可能找回差的面" — work out
by design, not by tuning, how much of each losing dimension is recoverable.

Status: **Phase A, design round.** No implementation. Acceptance lines here
are structural statements about what overhead remains and how each term
scales; the ratios against PostgreSQL are consequences of those statements,
never the target. (The house rule that produced this framing:
`feedback-design-from-own-model-not-attack` — do not let "beat X by Y%"
become an acceptance line.)

## 1. What the measurement established

Measured 2026-08-22 on lx64, median-of-3 throughout, both axes driven by one
harness (`bench/pgcompare.sh`, `bench/pgconc.sh`). Full numbers in the
scoreboard; only the load-bearing facts are restated here.

- **Under concurrency kevy already wins three of four shapes**, and the
  margin widens with client count: at 64 clients, point lookup 2.9×,
  indexed lookup 2.8×, single-row update 7.8× (at `everysec`). The p99
  column is more lopsided still — indexed lookup 957 µs against 5,876.
- **The list page is the one shape PostgreSQL wins on throughput** at every
  concurrency level (1.2–1.4×), while losing its tail (2,597 µs against
  4,132 at 64 clients).
- **The serial deficit on indexed lookup was a deployment shape.** Dropping
  `--threads` from 16 to 14 on a 16-core box moves idx p99 from 357 µs to
  100 µs. `bench/PERF-DECOMP-2026-08-04-idx-page-vs-pg18.md:646-652`
  predicted 98 µs and named the cause: run-queue latency from
  oversubscription, not engine work.
- **Memory is a real and structural loss**: 5,501 KB per MB of source CSV
  against PostgreSQL's 778 untiered; 280 against 62 at the 11.5 GB scale
  with tiering on.
- **At matched durability (`always`) the serial write is 2,900–5,200 µs
  against 850**, with p99 at 50 ms.

## 2. The model, and where the implementation departs from it

kevy's own claim for this surface is that **the access paths are declared,
not discovered** — `TABLE.DECLARE` enumerates the columns, their types, and
every index, and the engine refuses anything it was not told
(`.claude/plans/2026-07-03-rds-refugee-services.md`, Law 3). The whole
scope boundary rests on the declaration being complete.

The storage representation does not use it. A declared row is stored as the
fully general Redis hash: a hash table per row whose job is to answer, at
runtime, *which field is `dept`* — a question the declaration already
answered at DDL time.

That is the single structural observation this RFC turns on, and it has a
price in bytes, derived from source rather than estimated. For one row of
`id name dept age ts sku pad` (7 fields, 407 B payload) with two declared
indexes, at glibc chunk rounding and mid-cycle table occupancy:

| term | bytes | source |
|---|---|---|
| keyspace slot, amortised over the doubling cycle | ~153 | `kevy-map/src/map.rs:117-141`, `kevy-store/src/tests_tier.rs:626` |
| hash table allocation — **16 slots for 7 fields** | 816 | `map.rs:40` (`MIN_CAP`), `small_hash.rs:237` (`with_capacity(1)`) |
| `ArcInner` + `KevyMap` struct | 80 | `kevy-store/src/value.rs:186` |
| `pad` value heap chunk | 416 | `kevy-bytes/src/lib.rs:238` |
| six other values + all seven field names | 0 heap | inline `SmallBytes`, `kevy-bytes/src/lib.rs:193-206` |
| `t.sku` segment (tree + back + RowValues) | ~417 | `kevy-index/src/segment.rs:46-56` |
| `t.by_dept_ts` segment (+ `value_counts`) | ~400 | `segment.rs:49`, `composite.rs:95` |
| **total** | **~2280** | = 5.2× the 440 B CSV row |

Σ lands on the measured 5,501 KB/CSV-MB with no fudge term, so the
decomposition is closed (the ±20% reconciliation the perf methodology
requires; here it is ±4%).

**Memory is three independent problems, not one.** They scale differently
and only one of them is what this RFC is mostly about:

1. **Representation** — scales with columns-per-row × indexes-per-table.
   Fixed per row; addressable by using the declaration. §3.
2. **Residency** — scales with rows. *Already solved*: tiering holds 98.8%
   of 3M rows on disk at `used_memory` 0.42 GiB. Nothing to design.
3. **Fragmentation** — scales with allocation frequency. RSS is **7.2× the
   accounted bytes** in tiered mode. `kevy-alloc` exists for exactly this,
   is feature-gated off (`crates/kevy/Cargo.toml:25`), and has never been
   measured on this workload. **Measurement, not design** — in flight.

## 3. Axis A — a declared row deserves a declared representation

### What the declaration makes unnecessary

- **The per-row hash table.** 816 B of allocation carrying 336 B of live
  field data, for a lookup the schema already resolves. `MIN_CAP = 16` is a
  hard floor and `promote()` rounds straight to it.
- **The per-row field names.** Seven strings, identical across every row,
  stored inline in each row's slot array. They cost no allocation, but they
  cost slot space that the packed form would not have.
- **The `Arc` refcounts**, 16 B/row, supporting snapshot COW that is idle in
  steady state.
- **Six heap copies of the row key.** Each index stores the key three times
  — `segment.rs:131` (`back`), `:132` (`tree`), `rowvalues.rs:101`
  (`RowValues`) — and an inline copy already exists in the keyspace. Two
  indexes ⇒ six heap `Vec<u8>` of `row:NNNNNNN`, each its own malloc.
- **`value_counts` on a near-unique index.** `segment.rs:49` maintains a
  `BTreeMap` of one entry per distinct value, whose only consumer is the
  unique-fence duplicate count — on a range index over `ts` that is one
  B-tree entry per row (~80 B) to report a number that is structurally
  always zero.

### The shape this points at

A row of a declared table is a **packed value with a small offset table**,
and an index entry references it by a **dense row identifier** rather than
by the key bytes.

Neither idea is ours. PostgreSQL's heap tuple is exactly this — a header,
a null bitmap, and attributes laid out per the catalog's `pg_attribute`
order, with no per-tuple column names; and every PostgreSQL index stores a
`ctid`, a physical row pointer, never the primary key bytes. What this RFC
proposes is to take that arrangement for rows that kevy *was told about*,
while leaving every undeclared key on the general representation. The
contribution that is ours is the boundary: the general form stays the
default, and the declared form is an opt-in earned by the declaration.

### The ceiling, stated structurally

After such a change, a declared row's resident cost decomposes into exactly
these terms and no others:

| term | scales with | roughly |
|---|---|---|
| keyspace slot (key cell + `Entry` + control byte, amortised) | rows | ~153 B |
| packed payload + offset table, one allocation | column bytes | payload + ~2 B/column |
| index entry: ordered value + row-id | rows × indexes | value width + 8 B + container slot |

**No term remains that the declaration could have removed.** That is the
acceptance line. For the measured row it evaluates to ~865 B against
today's ~2280, i.e. the residual is ~38% — but the 38% is a consequence of
the three terms above, not a goal, and the design is right or wrong
according to whether a fourth term survives.

### What this does not touch

Residency. Every declared row still lives in RAM unless tiered. A packed
representation makes tiering cheaper per row; it does not replace it.

## 4. Axis B — the list page

The measured budget for `WHERE dept EQ ? RANGE ts ? ? LIMIT 20` at 16
shards, against a btree doing the same query:

| | kevy | btree |
|---|---|---|
| tree descents | 16 | 1 |
| index entries examined | ≤320 | 20 |
| rows crossing a shard boundary | 320 | 0 |
| rows sorted at the origin | 320, **full comparison sort** (`cmd_index_reduce/query.rs:160`) | 0 — index order is output order |
| row hydrations with `FIELDS` | ≤320 | 20, or 0 on an index-only scan |
| latency shape | max of 16 peer wake latencies | one descent + ~20 leaf steps |

**Structural: the fan-out is unprunable.** The index segment is a per-shard
object built by write hooks over the rows that shard owns, and row→shard is
the key hash. Rows with `dept=eng` are scattered across all shards by
construction; no shard subset can be proven empty. 16 descents are the price
of the partitioning, not waste in the query path.

**Not structural — four terms the current path spends that the model does
not require:**

1. The origin flattens 16 **already-sorted** runs into one `Vec`, sorts the
   whole thing, then truncates to 20. A 16-way merge stopping at the 20th
   element is O(20 log 16). (The doc comment at
   `cmd_index_reduce/query.rs:137` already claims a merge; the code does a
   sort.)
2. Hydration runs **per shard, before truncation** — up to 320 row reads of
   which ~300 are discarded. Hydrating after the merge costs 20.
3. `FIELDS` ignores the covering values the segment already holds
   (`segment.rs:92-107`) and probes the keyspace instead
   (`cmd_index_query/wire.rs:107-117`). **An index-only scan is on the table
   and not taken** — and the covering columns were already paid for in
   bytes (Axis A counts them).
4. `argv` is cloned per shard (`kevy-rt/src/exec_build.rs:102-105`) — ~160
   allocations to hand the same ten byte-strings to sixteen threads.

**Ceiling after those four:** 16 descents + ≤320 index-entry reads + 20
hydrations + a bounded merge. Descents and entry reads stay; boundary
crossings stay at 320 unless the shards exchange a watermark; hydration
drops 16×; the origin's sort disappears.

**The larger question, which is yours to rule on, not mine.** Partitioning
the index by its *leading declared column* instead of by row-key hash makes
`dept EQ x` touch one shard, which is what an RDS does and what removes the
16 descents. It breaks the stateless-shard model that
`.claude/plans/2026-07-03-v3-serving-engine.md` treats as load-bearing, and
it makes index placement depend on data distribution. This RFC does not
propose it; it records that the 16 descents are reachable only through it,
so that "16 descents" is never again described as a ceiling without naming
the door that was not opened.

## 5. Axis C — the write at matched durability

Three separable facts, from `crates/kevy-persist/` and `crates/kevy-rt/`:

1. **p99 = 50 ms is `park_timeout_ms`'s default value**
   (`kevy-rt/src/shard.rs:313-317`). The reply is held behind a durable
   watermark, and its release depends on a best-effort `waker.wake()` from
   the writer thread — documented as best-effort at
   `kevy-rt/src/aof_writer.rs:139-141`. A missed wake costs a full park
   timeout. **This is not the price of durability; it is the price of a
   dropped wake**, and it lands directly on client-visible p99.
2. **Group commit exists, but its unit is one socket read of one
   connection** (`kevy-rt/src/inbox.rs:86-89`,
   `kevy-rt/src/uring_io.rs:257-262`). Fifty connections each sending one
   un-pipelined write produce fifty events and therefore fifty fsyncs. It
   batches *pipelining*, not *concurrency*.
3. **In lane mode cross-connection batching does appear — as a side effect.**
   `aof_writer.rs:252-277` refuses a new fsync while one is in flight, so
   records arriving during a flush ride the next one. The batch size is
   whatever happened to arrive during the previous fsync's duration; there
   is no window. At low concurrency the batch is one, and the lane is then
   *worse* than synchronous because it adds ≥3 reactor iterations and 2
   worker round trips (`FINDING-2026-08-12-s2-always-cqe-gated.md:57-58`
   measures −49% at c=1).

**Structural: durability is per-shard by construction.** Each shard owns its
`Aof`, queue, lane thread and watermark. Sharing one fsync across shards is
not forbidden by policy — **there is no object in the structure that could
hold the batch**.

**Ceiling, stated structurally.** Under matched durability, a write costs:
one append, one fsync amortised over whatever shares it, and the wake
latency of the release path. Of those three, the second is bounded below by
the device and by the per-shard split; the third is bounded below by nothing
and is currently 50 ms at the tail. **The acceptance line for this axis is
that the third term stops appearing in the distribution at all** — the p99
should be a durability number, and today it is a scheduling number.

Whether the second term can be improved by a *window* rather than an
in-flight gate is the design question, and the answer must come after Item 2
measures what the current emergent batching already achieves under load —
that measurement is in flight and this RFC does not pre-judge it.

## 6. What is refused here

- No planner, no cost model, no automatic access-path selection. Nothing in
  §3–§5 chooses a path; the declaration still does.
- The general (undeclared) key representation does not change. A declared
  row's layout is earned by its declaration and applies to nothing else.
- No cross-shard transaction or coordinated durability point is proposed by
  §5; the per-shard split stays.
- "Beat PostgreSQL on the list page" is not an acceptance line anywhere in
  this document, and must not become one.

## 7. Sequencing

1. **Measure `kevy-alloc` on this workload** (Axis A term 3). Built, gated,
   off by default, never measured here. In flight.
2. **Measure `always` under concurrency** (Axis C term 2). The only
   comparison that can settle the write column. In flight.
3. **Axis B items 1–4.** Local to the reduce path and the hydration point;
   each is independently testable and independently revertible, and the
   suite already has `idxgate`/`servinggate` to hold them.
4. **Axis A.** The largest change, and the one that needs its own RFC with a
   storage-format section, a migration story, and a decision on whether the
   declared form is per-table opt-in. Not started here.
5. **Axis C term 1** (the dropped wake) is a defect, not a design item, and
   should be split out of this RFC as soon as it is reproduced under
   instrumentation.

## 8. Open, and honestly open

- The `everysec` write dip at 32 clients — 76,903 ops/s against 114,449 at
  8 and 229,768 at 64, reproduced across all three runs. Not noise, not
  explained. Suspected to involve the one-fsync-in-flight gate; unverified.
- `tiered` outperforms `everysec` under concurrency on the indexed lookup
  (110,419 against 85,183 at 64 clients, p99 957 against 4,705) while
  holding 98.8% of rows on disk. Counter-intuitive and unexplained.
  Candidate causes: smaller resident set improving locality, or AOF
  interference in `everysec`. Untested.
- `used_memory` under-reports: `HASH_SLOT_BYTES = 32` against a real 49 B
  slot, with `ArcInner` and the map struct uncharged (~344 B/row); the
  index formula's `48` has no gating test and is ~6× low. Any budget or
  tiering decision keyed off `used_memory` is working from a number below
  the truth. This is an accounting defect that should be fixed before Axis A
  makes decisions from those numbers.
