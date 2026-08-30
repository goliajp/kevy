# ZADD — Phase A decomposition (arena cell), v6 perf axis

Status: **Phase A in progress.** The attack candidate is named and priced
in part; the profile that splits its two halves is still building. Nothing
has been implemented. Phase B has not started.

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

### Priced by ablation, on a set where the tree costs something

200,000 members loaded and verified by `ZCARD` before **and** after the
run (the load silently produced an empty key on the first attempt through
`redis-cli --pipe`; the witness is why that reading was thrown away instead
of reported):

| | median ops/s | stdev | ns/op |
|---|---:|---:|---:|
| same score | 1,989,749 | 64,435 | 502.6 |
| varying score (`-r`) | 1,332,112 | 24,645 | 750.7 |

657,637 apart against a tolerance of 64,435 — ten times the band.

Against the one-member cell (333.9 ns/op, same protocol), a same-score ZADD
on a 200k set costs **168.7 ns/op more**.

**That 168.7 ns is not yet attributable.** At 200k the rank tree is deeper
*and* `by_member` is a 200k-entry hash whose lookups miss cache more often.
The number is the sum of both, and only the tree half is what the guard
would remove. Splitting them is what the pending profile is for.

---

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

---

## Open

1. **The profile that splits 168.7 ns into tree and hash.** Requires the
   `profiling` profile — release codegen with symbols. `strip = true` is set
   on `[profile.release]`, and a perf record of it resolves nothing but libc
   and the kernel: the first attempt here came back with 22.92% against a
   bare address. Cargo.toml says so directly, three profiles above where I
   read it: *"Three phrase-query profiles were spent finding that out."*
   Building now.
2. **Budget reconciliation.** The methodology wants stages summing to within
   ±20% of the measured wire time. Four stages are priced; the sum has not
   been closed against 340.2 ns/op.
3. **LPUSH**, the other narrow cell, is untouched here. Its arena cell grows
   without bound — at ~3M ops/s a 3-second window appends ~9M elements — so
   it is a different measurement with a different shape, and it needs its own
   pass rather than a paragraph in this one.

## Incidental, from the box rather than the code

The kernel side of the first profile resolved fine and shows the audit
subsystem at roughly 8.6% of all samples (`audit_reset_context` 3.49%,
`auditd_test_task` 1.55%, `__audit_syscall_entry` 1.52%,
`__audit_syscall_exit` 1.20%, `syscall_trace_enter` 0.84%). It taxes every
syscall on this box. It applies equally to all four engines in the arena, so
no ratio in the ledger is distorted by it, but absolute ops/s numbers from
lx64 carry it.
