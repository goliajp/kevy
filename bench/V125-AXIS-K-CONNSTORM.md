# Axis K — connection storm (c = 3 000 / 5 000 / 8 000 / 10 000)

> **v1.25 outcome — cliff RESOLVED**
>
> Phase A decomposition: `.claude/notes/v125-deco-axis-k-c10000.md`.
>
> **R3 ★ dramatically flipped finding**: the historical "valkey 10-
> io_threads amortises kernel per-flow cost better" framing in this
> doc was wrong. The cliff was a **kevy-internal positive-feedback
> meltdown**, not a kernel scaling property:
> - Linux multishot recv terminates with `F_MORE` clear on `-ENOBUFS`
> - `PBUF_ENTRIES=128` shared across N=10 000 conns → every burst
>   exhausted the pool → multishot tore down
> - Each torn-down recv needs a re-arm SQE; `URING_ENTRIES=256`
>   capped re-arms to 256/iter → ~38 reactor iters per useful 128-op
>   batch → throughput collapsed to 270 rps
> - valkey side-steps the entire failure mode by using readiness
>   (epoll) instead of completion; no shared limited-resource analog
>   to PBUF exists in valkey's per-client-querybuf model.
>
> The bonus puzzle ("backlog 1024 → 16384 made c=8 000 SET WORSE,
> not better") had its own R3 ★: bigger backlog meant AOF flushes
> got more spurious wake-ups, stretching the bad-budget window.
> Trigger was PBUF, AOF was the amplifier. Backlog was a distractor.
>
> **Shipped in v1.25**:
> - G1 (`01948ca`) — `PBUF_ENTRIES: 128 → 4096` + `URING_ENTRIES: 256
>   → 2048`. Two-line change. Measured:
>   - c=10 000 t=1 SET: **270 rps → 120 178 rps (+44 511×)**
>   - c=10 000 t=1 GET: **400 rps → 119 460 rps (+298×)**
>   - vs valkey c=10 000: kevy 120 k vs valkey 117 k = 103 % WIN
>   - vs valkey c=3 000…8 000: every cell 100-138 % (kevy wins or ties)
> - `923b928` — Negative-learning case: reverted the polish-trap
>   backlog bump (`tcp_listen(.., 16384)` → `1024`). Logged in this
>   doc's "Negative-learning case" section below as the canonical
>   `.claude/rule/perf-vs-foss.md` R1-R3 violation example.
>
> **Deferred to v1.26 (optional further wins)**:
> - K4 ready-set bitmap arm-loop — `O(active)` per-iter instead of
>   `O(N=10 000)` Vec walk. Currently bench shows G1 alone makes t=1
>   the winning config at every conn count, so K4 is not blocking.

---

# Historical body (pre-v1.25 framing — cliff cause was wrong)

Extends the threads-sweep finding (`V125-THREADS-FINDING.md`)
beyond c=2 000 to confirm where the t=1 sweet spot ends. lx64,
mitigations=off, `ulimit -n 200000`, n=2 000 000 per run.

## Raw data (rps, best of 2 runs)

| c     | kevy t=1 SET | kevy t=2 SET | kevy t=4 SET | valkey SET | redis SET |
|-------|--------------|--------------|--------------|------------|-----------|
| 3 000 | **143 493**  | 136 472      | 132 345      | 142 430    | 121 699   |
| 5 000 | **138 716**  | 116 394      | 103 082      | 106 900    |  94 144   |
| 8 000 | **104 134**  | 101 673      | 100 306      | 100 528    |  95 850   |
|10 000 | **270**      | **113 837**  | 109 403      | 117 626    | 101 302   |

| c     | kevy t=1 GET | kevy t=2 GET | kevy t=4 GET | valkey GET | redis GET |
|-------|--------------|--------------|--------------|------------|-----------|
| 3 000 | **147 037**  | 134 798      | 131 130      | 143 812    | 118 231   |
| 5 000 | **137 391**  | 109 164      |  87 218      | 122 362    |  96 061   |
| 8 000 | **103 837**  | 101 466      |  97 399      | 101 611    |  95 717   |
|10 000 | **400**      | **113 585**  | 107 585      | 117 357    | 102 150   |

## Best-kevy vs valkey

| c     | best kevy | best t | valkey | kevy / valkey | verdict |
|-------|-----------|--------|--------|----------------|---------|
| 3 000 | 143/147   | t=1    | 142/144| 101 %/103 %    | ⚠ tied  |
| 5 000 | 138/137   | t=1    | 107/122| **130 %/112 %**| ✅ ≥120% (SET only) |
| 8 000 | 104/103   | t=1    | 100/102| 104 %/102 %    | ⚠ tied  |
|10 000 | 113/113   | t=2    | 118/117| 96 %/97 %      | ❌ LOSS |

## Two findings

### 1. t=1 is robust up to c=8 000 — sweet spot extends further

At c=3 000-8 000, `--threads 1` consistently outperforms t=2 / t=4,
matching the c=50-2 000 sweep result. The "single shard wins on
loopback" rule extends to **all conn counts below 10 000**.

At c=5 000 SET, kevy t=1 (138 k) is **30 % faster than valkey**
(107 k) — the largest single-axis advantage in the connection
range. valkey's main-thread dispatcher serialises commands across
its 10 io_threads; at this load it becomes the bottleneck while
kevy's single shard busy-polls without coordination cost.

### 2. **t=1 cliffs hard at c=10 000** — discovered bug-level limit

```
c=10 000 t=1: SET=270 rps  GET=400 rps   ← effectively dead
c=10 000 t=2: SET=113 k    GET=113 k     ← back to normal
```

Three orders of magnitude collapse, **not gradual degradation**.
Likely root cause: with `accept` running on the single busy-poll
shard, the accept queue can't drain fast enough vs incoming
connection attempts from 10 000 simultaneous redis-benchmark
clients. Once the accept backlog overflows, new connects retry,
piling on more SYN load that further starves the busy-poll loop.

**Implication for v1.25**: the matrix default `--threads 1` is
correct for c ≤ 8 000 (the entire `redis-benchmark`-bench range).
For c ≥ 10 000 production workloads, document `--threads 2+`.

### 3. At c=10 000 even t=2/4 lose to valkey

c=10 000 valkey 117 k vs kevy-best t=2 113 k = **kevy at 96 %**.

This is the **first scenario in the v1.25 sprint** where kevy
honestly loses to valkey. At extreme conn counts, valkey's 10-
io_threads design amortises kernel per-flow scheduling cost
better than kevy's t=2 (where each shard owns 5 000 conns and
its busy-poll iterates them all).

The fix would require:
- the ready-set bitmap arm_conns refactor (originally proposed
  in Axis E), making per-iter cost O(active) rather than O(N)
- or a per-shard accept thread separate from the busy-poll core

Both are real engineering work; for v1.25 we document the limit
and ship.

## What this means for v1.25 positioning

- `--threads 1` default is safe for **c=1 to c=8 000** — covers
  ALL of redis-benchmark's standard range and 99 %+ of real
  production workloads.
- At c=10 000+ document `--threads 2` and note kevy is at
  parity-minus (96-97 %) vs valkey — honest disclosure.
- t=1 cliff at c=10 000 should be a future-sprint fix item.

## Reproduce

```bash
ssh lx64
bash /tmp/axis_k_connstorm.sh
```

## Status

⚠ **Mixed**: kevy t=1 wins decisively at c=5 000 (130 % SET) and
holds parity at c=3 000 / 8 000. The c=10 000 cliff is a known
limit; t=2 recovers but loses 3-4 % to valkey.

## Negative-learning case (2026-06-22, post-methodology read)

After the initial run, I tried a R1-R2 violation surgical polish:
bumped `tcp_listen(.., 1024)` → `tcp_listen(.., 16384)` in
`crates/kevy-rt/src/runtime.rs` on the hypothesis that the c=10 000
t=1 cliff was accept-queue overflow.

**Prediction**: backlog bump fixes the cliff.

**Measurement** (re-bench with backlog=16384, kernel-capped to
somaxconn=4096):

```
kevy c=8000 t=1  SET = 6 921       ← was 104 134 in prior run (-93 %)
kevy c=8000 t=1  GET = 102 833     ← roughly unchanged
kevy c=8000 t=2  SET = 100 522     ← unchanged
valkey c=8000    SET = 101 040     ← unchanged
kevy c=10000 t=1 SET = <hung — bench killed after hours>
```

**Refutation**: the polish was actively harmful — c=8 000 t=1 SET
crashed from 104 k to 6.9 k (-93 %). c=10 000 t=1 SET became so
slow it never completed 2 M requests in the bench window.

The fix was reverted in this commit. The root cause of the c=10 000
t=1 cliff (and now the c=8 000 SET-only regression under bigger
backlog) is **not understood** — it needs Phase A decomposition
(see `.claude/notes/v125-deco-axis-k-c10000.md`, task #82), not
more polish.

**Lesson for the methodology log**: this is a textbook
`.claude/rule/perf-vs-foss.md` R1-R3 violation. Two rounds of
polish would have caught it sooner; one round + the trigger-word
mental check ("backlog should fix it, *probably*") was already
enough to say "no, decompose". I didn't, paid hours of zombie
bench time, almost broke the shared lx64 perf box (caught by
[[feedback-no-zombie-background-shells]] reflex).

Revert commit message: revert backlog bump; the cliff demands
decomposition, not surgical polish.
