# ZADD — Phase A decomposition (arena cell), v6 perf axis

Status: **Phase A and Phase B complete for ZADD.** The candidate was named
and priced by ablation, reconciled to within 0.9% of the measured wire time,
implemented, and measured. It holds — and one line of the Phase A prediction
was wrong, recorded below rather than quietly dropped. LPUSH, the other
narrow cell, is not covered.

Opened because the v6 ROADMAP's one substantive open perf item reads:

> **性能轴一次未碰**。6.0.0 arena：对 Redis 8 GET 1.26x / SET 2.58x /
> INCR 1.98x / SADD 1.52x / HSET 1.40x / LPUSH 1.07x，而 **ZADD 落在噪声带
> 里**（差 80,594，容差 82,849）。攻最窄的两格需要 decomposition 与独占盒时间。

---

## Pre-Phase-A gate: the stated target fails it

The methodology's first gate is that a gap must be established before it is
attacked. The ledger's own gap rule is
`|kevy - other| <= max(stdev_kevy, stdev_other) => NOISE`, and the 6.0.0
entry applies it to ZADD itself: 80,594 against a tolerance of 82,849.

**So "kevy ZADD vs Redis 8" is not a gap.** Attacking it would be attacking
noise, which is the failure this gate exists to prevent. LPUSH's 1.07x
(191,922) does clear the band and remains a real, narrow lead.

The target had to come from somewhere else. It came from the same table
read down a column instead of across a row:

| verb | kevy | as a fraction of kevy's own GET |
|---|---:|---:|
| GET | 7,342,979 | — |
| ZADD | 2,939,276 | **0.40** |
| LPUSH | 2,996,776 | 0.41 |

Redis 8 falls from 5,835,267 to 2,858,682 on the same two verbs — a factor
of 2.04. kevy falls by 2.50. Both engines slow down on collection writes;
kevy slows down more, which is why a 1.26x lead becomes a tie. That ratio
is measured by one harness in one run with per-cell stdevs of 1-2%, so
unlike the vs-Redis gap it is not inside anything's band.

Per-op, subtracting the shared path:

| | ns/op | minus GET | 
|---|---:|---:|
| kevy GET | 136.2 | — |
| kevy ZADD | 340.2 | **204.0** |
| Redis GET | 171.4 | — |
| Redis ZADD | 349.8 | **178.4** |

kevy's base path is 35.2 ns/op cheaper and its ZADD-specific work is
25.6 ns/op dearer.

---

## S01 — what the arena's ZADD cell actually does

Established first, because everything downstream depends on it. A kevy
server, `redis-benchmark -t <verb> -n 2000` for each of the seven arena
verbs, then the keyspace read back:

| cell | key | after 2,000 ops |
|---|---|---|
| GET / SET | `key:__rand_int__` | one string |
| INCR | `counter:__rand_int__` | one string |
| **ZADD** | `myzset` | **1 member** |
| **SADD** | `myset` | **1 member** |
| **HSET** | `myhash` | **1 field** |
| LPUSH | `mylist` | 2,000 elements |

Without `-r`, redis-benchmark does not substitute `__rand_int__`; it sends
the literal. So ZADD writes **the same member at the same score, forever**,
and the sorted set never exceeds one entry. SADD and HSET are the same
shape. LPUSH is the only collection cell that grows.

This retires a conclusion that was on the books. The 2026-08-08
decomposition profiled zadd and found `binary_search<(Score, SmallBytes)>`
at 17.02% self-time, concluding "zadd is tree-walk-bound". That was
measured at `-P 256` on 2 shards — a different shape. **In the arena cell
there is no tree to walk**: the sorted set holds one member. The earlier
finding is not wrong, it simply does not describe this cell, and the two
were about to be conflated.

---

## S02 — the arena's member does not fit the inline encoding

`SmallZSetData` (crates/kevy-store/src/small_zset.rs) packs each entry as
`[score: f64][len: u8][member]` into a 22-byte buffer, so

```
SMALL_ZSET_MEMBER_MAX = SMALL_ZSET_BUF_CAP - 9 = 13
```

The arena's member is `element:__rand_int__` — **20 bytes**. It cannot be
inlined, and every ZADD in that cell runs on the promoted representation.

The file's own doc comment says the opposite:

> For the `redis-benchmark -t zadd` default literal-member shape
> (`element:__rand_int__`, 20 bytes), only the **single-element** ZADD
> pattern fits inline (9 + 1 + 12 = 22 — exact fit).

`9 + 1 + 12` counts the member as 12 bytes — the length of `__rand_int__`
without the `element:` prefix it is written beside. The correct sum is
`9 + 20 = 29 > 22`. The comment names this exact benchmark and reaches the
wrong conclusion about it.

### The cliff, measured

Arena protocol (`-c 50 -P 16`, 8 threads, server cores 0-7, client 8-15,
throughput from the server's own `total_commands_processed` over a timed
window), member length swept across the boundary, **rounds interleaved** so
time-drift cannot masquerade as a step:

| member | median ops/s | stdev |
|---:|---:|---:|
| 8 B | 3,531,325 | 12.2% |
| 12 B | 4,151,958 | 2.8% |
| **13 B** | **4,003,647** | 6.7% |
| **14 B** | **3,003,441** | 3.5% |
| 16 B | 3,091,713 | 2.5% |
| 20 B | 3,019,922 | 4.8% |

A step at exactly 13→14, flat on both sides — not a per-byte gradient, or
20 B would read below 14 B, and it does not.

### ...and the control that shrank it

That sweep used a different key per length (`zk8`, `zk12`, ...). Under a
single-key load only **one shard owns the key**; a different key is a
different owner. The 8 B row — slower than 12 B and with four times the
relative spread — is what that looks like.

Re-run with one key name for every length and a `DEL` between cells, so the
owner shard is held constant:

| member | median ops/s | stdev |
|---:|---:|---:|
| 12 B | 3,452,701 | 5.9% |
| 13 B | 3,478,447 | 6.1% |
| **14 B** | **2,966,490** | 3.1% |
| 16 B | 3,061,006 | 2.8% |

Still a step, still at the boundary, and **−14.7% rather than −25%**. Half
of the first reading was shard placement. 12 and 13 are one level, 14 and
16 are another; the two levels are 49.6 ns/op apart for a set holding one
member.

### Why the obvious fix is not available

Raising `SMALL_ZSET_BUF_CAP` is blocked by a deliberate, documented budget:

```rust
const _: () = {
    // Don't let future variants undo box-collection's Entry-48B win.
    assert!(core::mem::size_of::<Value>() <= 32);
};
```

`SmallZSetData` is exactly 24 bytes and `Entry` exactly 48. Zsets get 13
bytes where sets and lists get 21 because the f64 score eats 8 of the 22.
That is a real design observation — a zset member of 13 bytes is short
next to a UUID (36), a ULID (26), or `user:1234567` — but it is a memory
trade the owner has already priced, not something to change in a perf pass.

---

## S03 — the attack candidate: an unchanged score still rewrites the index

`ZSetData::insert` (crates/kevy-store/src/value.rs:81):

```rust
pub(crate) fn insert(&mut self, member: &[u8], score: f64) -> bool {
    let is_new = match self.by_member.insert(SmallBytes::from_slice(member), score) {
        Some(old) => {
            self.by_score.remove(&(Score(old), SmallBytes::from_slice(member)));
            false
        }
        None => true,
    };
    self.by_score.insert((Score(score), SmallBytes::from_slice(member)));
    is_new
}
```

When the member is already present **at the same score**, the `remove` and
the `insert` are of the same key: the rank tree ends in the state it began.
The operation pays a full B-tree removal, a full B-tree insertion, and two
extra `SmallBytes::from_slice` constructions to change nothing.

This is not a benchmark-shaped case. Re-adding a member at an unchanged
score is what an idempotent upsert does, what a retry does, and what a
leaderboard write does whenever the value has not moved.

### Priced by ablation: the hash is the control

`ZSCORE` reaches `by_member` and stops. `ZADD` reaches `by_member` and then
the ordered index. Both go through the same parse and dispatch. Measured on
the same server across sizes that span both encodings — arena protocol,
median-of-5, cardinality read back before **and** after every sweep (the
first attempt at loading 200k produced an empty key through
`redis-cli --pipe` and the witness is why that reading was discarded rather
than reported):

| members | encoding | ZADD ns/op | ZSCORE ns/op | ZADD − ZSCORE |
|---:|---|---:|---:|---:|
| 1 | Flat | 330.7 (±4.7%) | 159.2 | 171.5 |
| 100 | Flat | 370.5 (±4.7%) | 159.1 | 211.4 |
| 2,000 | Flat | 418.7 (±5.4%) | 151.7 | 266.9 |
| 8,000 | Flat | 476.4 (±2.5%) | 155.1 | 321.3 |
| 200,000 | Seg | 492.9 (±2.3%) | 150.2 | 342.8 |

**`ZSCORE` is flat at 150-159 ns/op across five orders of magnitude.** The
member hash does not care how many members there are, which is what a hash
is for — and it is the control that makes the rest of the table readable.

`ZADD` is not flat. It costs 162.2 ns/op more at 200,000 members than at 1,
and 145.7 ns/op more at 8,000 than at 1 — the latter entirely within the
Flat encoding, so it is one code path being asked to do more work, not two
code paths being compared.

The write path, the entry lookup, the COW and the propagation bookkeeping
are all the same at n=1 and n=8,000. The hash is measured constant. **So
the size-dependent term is the ordered index and nothing else** — which is
precisely the remove-and-insert pair that a same-score ZADD does not need.

| | n=1 | n=8,000 | n=200,000 |
|---|---:|---:|---:|
| ZADD, total | 330.7 | 476.4 | 492.9 |
| — of which grows with the set | 0 (by definition) | **145.7** | **162.2** |
| — as a share of the op | — | **30.6%** | **32.9%** |

### What this predicts, and what it does not

A guard that skips the index work when the score is unchanged removes the
size-dependent term outright, plus a fixed part it cannot separate from the
rest without being implemented. So:

- at 8,000 members: **≥145.7 ns/op of 476.4**, i.e. 2,099,000 -> ≥3,024,000
  ops/s, **+44% or better**;
- at 200,000 members (Seg): ≥162.2 ns/op of 492.9, **+49% or better**;
- **at the arena's cell, n=1: almost nothing.** The size-dependent term is
  zero there by construction. Only the fixed cost of calling remove and
  insert on a one-node tree, and two `SmallBytes`, are on the table.

That last line is the answer to the question the ROADMAP actually asked,
and it is worth stating plainly: **the cell the roadmap points at is not
where this cost lives.** The arena's ZADD writes to a sorted set of one
member. Real sorted sets are leaderboards, time indexes, priority queues —
thousands to millions of members — and it is at those sizes that a third of
every same-score ZADD is spent taking an entry out of the index and putting
it back unchanged.

The Seg encoding has the same shape and one extra cost. `SegZSetData::insert`
(crates/kevy-store/src/zset_seg.rs:80) removes and re-inserts just as
unconditionally, and reaches its segment through
`Arc::make_mut(&mut self.segs[si])` — so under a live snapshot a same-score
ZADD can deep-clone a segment of up to `ZSEG_CAP` = 512 entries in order to
put back exactly what was in it.

### A correction to this document's own first pass

An earlier reading here priced the candidate at "168.7 ns/op on a 200k set"
by subtracting the one-member cell from the 200k one. That subtraction
spans the `Z_PROMOTE` = 16,384 boundary: the one-member cell is `ZSet`
(Flat) and the 200k cell is `SegZSet`. It was two encodings being
differenced as though they were one, which is the same mistake this
document opens by pointing out in the 2026-08-08 profile. The 8,000-member
row exists because it is on the same side of that boundary as n=1.

## S04 — the guard has a correctness precondition, and it exposes a latent defect

```rust
#[derive(Clone, Copy, PartialEq)]   // derived: f64 ==
pub struct Score(pub f64);
impl Eq for Score {}
impl Ord for Score {
    fn cmp(&self, other: &Self) -> Ordering { self.0.total_cmp(&other.0) }
}
```

`PartialEq` is derived from `f64`'s `==`; `Ord` is `total_cmp`. **They
disagree.** `-0.0 == 0.0` is true, while `total_cmp` orders `-0.0` before
`0.0`. Rust requires of an `Ord` key that `a == b` iff `a.cmp(b)` is
`Equal`, and `Score` breaks that.

It is latent today only because nothing compares two `Score`s with `==` —
the current `insert` reaches the tree exclusively through `Ord`, and its
unconditional remove-then-insert is, accidentally, what keeps it correct.

Reachable, on a running server:

```
ZADD z2 -0 a ; ZADD z2 0 b
ZRANGE z2 0 -1 WITHSCORES  ->  a 0 b 0     (a sorts first: -0.0 < 0.0)
ZADD z -0 m ; ZADD z 0 m   ->  1, then 0   (accepted, then updated)
ZADD z3 nan m              ->  ERR value is not a valid float
```

NaN is refused at the parser, so the comment's "Redis scores are never NaN"
holds. `-0.0` is not refused and is a distinct sort key.

So a guard written the obvious way —

```rust
Some(old) if old == score => false,          // WRONG
```

— would skip the update for `-0 -> 0`, leaving `by_member` holding `0.0`
while `by_score` still holds the `-0.0` key: the two indexes of one sorted
set disagreeing about a member. The guard must compare through the same
order the tree uses (`total_cmp`), not through `==`.

`Score`'s derived `PartialEq` is worth fixing on its own terms regardless
of what this decomposition concludes.

And pulling that thread found a divergence underneath it: Redis treats
`-0` and `0` as one score and ties on the member, kevy's `total_cmp` orders
the negative first, so `ZADD z 0 a; ZADD z -0 b; ZRANGE z 0 -1` answers
`a b` there and `b a` here. Reproduced against `redis:8` on the same host
and written up separately — it is not caused by the guard, is not fixed by
it, and changing observable ordering does not belong inside a perf change.
See `bench/FINDING-2026-08-30-negative-zero-is-its-own-score-here.md`.

---

## Budget reconciliation

The methodology asks that the stages sum to the measured wire time within
±20%. For ZADD on an 8,000-member Flat set, where every term below was
measured on the same server in the same session:

| term | ns/op | how it was measured |
|---|---:|---|
| parse, dispatch, entry lookup, member hash, reply | 155.1 | `ZSCORE` on the same key |
| write path + hash insert over get + 2 extra `SmallBytes` + fixed index calls | 171.5 | `ZADD` − `ZSCORE`, both at n=1 |
| ordered-index work that scales with the set | 145.7 | `ZADD` at n=8,000 − `ZADD` at n=1 |
| **sum** | **472.3** | |
| **measured** | **476.4** | |

**0.9% apart.** No stage is missing.

The prediction that follows is stated as separately measurable quantities
rather than one number, because the one-number form is what the SPG round
got wrong five times out of six: the third row is what the guard removes
in full, the second row is what it dents, and the first row is untouched.
If a Phase B lands and ZADD at n=8,000 does not reach ~3.02M ops/s, one of
those three rows is wrong and the ablation says which one to re-measure.

## Open

1. **Why the arena cell's gain is unstable between runs.** The base column
   reproduces to 0.2% across two sessions; the guard column moves by 10%.
   A cheaper operation is more exposed to whatever else bounds that cell,
   and this decomposition has not said what that is. Until it does, the
   arena number is quoted as a range with the better-powered run as the
   figure.
2. **The row the guard does not touch has a named candidate in it.** Of the
   171.5 ns at n=1, the guard removes 59.1 and leaves 112.4 — write path,
   hash insert over get, and `SmallBytes`. Part of that is a double lookup
   `zadd_one` does on every call:

   ```rust
   if self.zset_value_for_set(key)?.is_none() {
       return Ok(self.zadd_create(key, m, score));
   }
   let v = self.zset_value_for_set(key)?.expect("present and a zset");
   ```

   The key is found once to ask whether it is there and again to get it, and
   `account_delta` looks it up a third time. `list_push_one` has the same
   shape, and `lpush` adds a fourth lookup through `list_len` to report the
   new length — so a single-value LPUSH on an existing list can reach four
   hash lookups where one would do.

   It is not a three-line change: the pattern is what the borrow checker
   leaves you with when a mutable borrow cannot be held across the question
   and the answer. Priced before written, like this one — an ablation
   against `ZSCORE` will not separate it, because `ZSCORE` looks up once too.

3. **LPUSH**, the other narrow cell, is untouched here. Its arena cell grows
   without bound — at ~3M ops/s a 3-second window appends ~9M elements — so
   it is a different measurement with a different shape, and it needs its own
   pass rather than a paragraph in this one.
4. **The 13-byte inline ceiling** is priced (14.7% at the boundary) but not
   actionable inside the `size_of::<Value>() <= 32` budget that keeps `Entry`
   at 48 bytes. Whether a zset member deserves more inline room than a
   `user:1234567` is a memory trade, and the owner has already priced the
   other side of it.

## Phase B — measured

Landed as `Score` made consistent with its own `Ord` (its own commit, with
red-green tests), then the guard in `ZSetData::insert` and
`SegZSetData::insert`, comparing through `Score` and never `f64`.

Two binaries built from the same source tree on the bench box, differing
only in the guard (md5 checked — a build that does not change the binary is
not a build), then measured **interleaved in one session**, because the
ledger's own entry for 6.0.0 records this box moving 5-10% between days.

### The guard, kevy against itself

Arena protocol, median-of-5 per cell, rounds alternating between the two
binaries, cardinality verified before and after each load:

| members | encoding | base | guard | Δ | tolerance | |
|---:|---|---:|---:|---:|---:|---|
| 1 | Flat | 3,093,924 | 3,786,703 | +692,779 | 338,180 | **+22.4%** |
| 8,000 | Flat | 2,231,477 | 3,408,582 | +1,177,105 | 291,943 | **+52.8%** |
| 200,000 | Seg | 2,033,624 | 3,158,316 | +1,124,692 | 127,315 | **+55.3%** |

All three clear the band — by 2.0x, 4.0x and 8.8x respectively. The two
large-set numbers beat the Phase A prediction of +44% and +49%.

### Where the prediction was wrong

Phase A said of the one-member cell: *"almost nothing. The size-dependent
term is zero there by construction."* It measured **+22.4%**, or 59.1 ns/op.

The budget said which row that has to come out of. Row two —
"write path + hash insert over get + 2 extra `SmallBytes` + fixed index
calls", 171.5 ns at n=1 — was carrying a fixed index cost I described as
present but treated as negligible. It is 59.1 ns of that 171.5: two
`RankTree` calls and two `SmallBytes` on a one-node tree. Splitting row two:

| | ns/op | removed by the guard |
|---|---:|---|
| write path, hash insert over get, 2 `SmallBytes` | 112.4 | no |
| fixed index calls | 59.1 | **yes** |

This is the whole reason the prediction was written as three measurable rows
rather than one number: the miss names its own row.

### What else a skipped write could have broken, and did not

Three things downstream of a ZADD could plausibly have depended on the
store actually touching the index. Each was read rather than assumed:

- **AOF and replication.** `post_write_housekeeping(args, meta)` runs
  whenever `meta.is_write`, which comes from `is_write_verb` — a static
  property of the verb. It takes the raw arguments, never the store's
  return value, so a ZADD that changes nothing is still recorded and still
  reaches a replica.
- **`WATCH`.** The version bump is `self.store.bump_if_watched(&args[idx])`
  in the runtime's dispatch layer, not inside `insert`. It bumped before
  this change on a same-score write and it bumps now.
- **`ZADD … CH` and the flag variants.** They call `Store::zadd`, so they
  do reach the guard — but `zadd_flags` already short-circuits an unchanged
  score itself (`if *score != old`), and where it does call through, the
  guard returns exactly the `is_new` the old code did. `ZADD … INCR` with a
  delta of zero now skips the index, which is correct and which the `-0.0`
  case makes non-trivial: `-0.0 + 0.0` is `+0.0`, a different key under
  `total_cmp`, so that one is a real update and the guard lets it through.

### The seven arena cells, and nothing else moved

Same interleaving, kevy against kevy, median-of-5:

| verb | base | guard | |
|---|---:|---:|---|
| GET | 7,252,269 | 7,512,772 | +3.6% |
| SET | 6,758,714 | 6,693,121 | NOISE |
| INCR | 6,414,602 | 6,391,528 | NOISE |
| LPUSH | 3,082,703 | 3,085,622 | NOISE |
| SADD | 5,413,682 | 5,356,876 | NOISE |
| HSET | 4,125,795 | 4,010,546 | NOISE |
| **ZADD** | 3,023,834 | 3,649,594 | **+20.7%** |

GET's +3.6% sits 1.5x outside its band and has no mechanism — the guard is
not on GET's path, and code layout is the only way it could reach. It is
recorded, not claimed.

### The arena cell's effect size is not stable, and the better-powered run wins

A second ZADD comparison, exclusive box, nine interleaved rounds per engine,
tighter bands:

| | median | stdev |
|---|---:|---:|
| kevy guard | 3,312,723 | ±196,844 (5.9%) |
| kevy base | 3,028,399 | ±218,139 (7.2%) |
| Redis 8 | 2,875,747 | ±61,669 (2.1%) |

| | Δ | tolerance | |
|---|---:|---:|---|
| guard vs base | +284,324 | 218,139 | **1.09x** |
| guard vs Redis 8 | +436,976 | 196,844 | **1.15x** |
| base vs Redis 8 | +152,652 | 218,139 | **NOISE** |

The base column reproduces across the two runs to within 0.2% (3,023,834 and
3,028,399); it is the **guard** side that moves, 3,649,594 against
3,312,723. So the arena cell's gain is somewhere in +9% to +21% and this run,
with nine rounds on a quiet box, is the one to quote: **+9.4%**.

The bottom row is the point. `base vs Redis 8` reproduces the 6.0.0 ledger's
verdict exactly — NOISE, a tie — and the guard turns it into a lead that
clears its band by 2.2x. That is the ROADMAP's cell, moved.

The large-set numbers are where the change actually pays, and they are not
in question: +52.8% and +55.3%, at 4.0x and 8.8x their tolerances.

### A measurement thrown away

Between those two runs there is a third that is not reported, because it was
taken while the full four-engine `arena.sh` was **still running** on the same
cores. It read 45-48% stdev and roughly half the throughput of every other
run. A rate measured under one's own interference is not a rate; the fix was
to wait for `gap rule` to appear in the arena output, then for the load
average to come back under 1.2, and measure again.

### Full arena, four engines, same session, guard binary

| verb | kevy 6.1.0+guard | Redis 8 | valkey 9.1.1 | Dragonfly |
|---|---:|---:|---:|---:|
| GET | 7,320,368 | 5,681,931 | 3,037,838 | 2,894,780 |
| SET | 6,393,582 | 2,522,055 | 1,665,513 | 1,941,631 |
| INCR | 6,314,125 | 3,384,681 | 2,232,867 | 2,073,333 |
| SADD | 5,380,960 | 3,653,358 | 2,426,068 | 1,490,047 |
| HSET | 4,233,639 | 2,959,818 | 2,005,480 | 1,438,887 |
| LPUSH | 2,988,692 | 2,872,658 | 1,967,287 | 1,247,830 |
| ZADD | 3,187,200 | 2,811,568 | 1,886,907 | 1,502,123 |

kevy's ZADD cell carried an 11.7% sample stdev in this run, which is why the
vs-Redis ratio is quoted from the nine-round comparison above and not from
this table. The rest of the table is the like-for-like context.

## Incidental, from the box rather than the code

The kernel side of the first profile resolved fine and shows the audit
subsystem at roughly 8.6% of all samples (`audit_reset_context` 3.49%,
`auditd_test_task` 1.55%, `__audit_syscall_entry` 1.52%,
`__audit_syscall_exit` 1.20%, `syscall_trace_enter` 0.84%). It taxes every
syscall on this box. It applies equally to all four engines in the arena, so
no ratio in the ledger is distorted by it, but absolute ops/s numbers from
lx64 carry it.
