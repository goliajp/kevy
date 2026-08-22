# FINDING 2026-08-23 — a 50 ms write p99 at one client, and the loop that produces it

**Status**: hypothesis with a mechanism and a configuration-only test that
decides it. No code changed yet. Named as ship-regardless in
`.claude/rfcs/2026-08-23-v5.4-use-the-declaration.md` §6.

## The number

`appendfsync always`, one client, three independent runs:

| run | conc | write p50 | write p99 | write mean |
|---|---:|---:|---:|---:|
| conc-always-r1 | 1 | 2,903 | **50,197** | 6,251 |
| conc-always-r2 | 1 | 3,067 | **51,477** | 8,020 |
| conc-always-r3 | 1 | 3,133 | **50,304** | 8,163 |
| conc-always-r3 | 8 | 5,523 | 15,195 | 6,181 |
| conc-always-r3 | 32 | 5,728 | 12,285 | 6,321 |
| conc-always-r3 | 64 | 5,356 | 10,960 | 6,022 |

All µs. Three properties fix the shape of the cause:

1. **50 ms is `park_timeout_ms`'s default**, to three digits, three times.
2. It appears **only at one client**. More load makes p99 *better*, which
   no queueing or device explanation does.
3. **mean far exceeds p50** (8,163 against 3,133), so this is not one
   outlier — roughly a tenth of requests pay a full park.

## The loop

Per iteration, `crates/kevy-rt/src/uring_reactor.rs`:

```
159  uring_arm_conns(…)          ← opens the Always gate, submits the write SQE
160  submit_and_wait(0)
162  reap CQEs
190  OP_AOF → uring_aof_on_cqe   ← advances durable_watermark
312  uring_aof_tick
     … spin or park
```

Under `always` a reply's bytes sit in `conn.output` behind
`UringConn::held_watermark` (`uring_arm.rs:194-201`) and are released by the
*next* arming pass — the code says so: "release is re-checked every pass
(≤1 pass of latency after the CQE)". One more pass is the whole contract.

The pass is not guaranteed. `io_work` is set by `OP_RECV`, `OP_WRITE` and the
other socket arms; **`OP_AOF` is not one of them** (`uring_reactor.rs:174-190`),
deliberately, because park-administrative CQEs must not reset the idle
ladder. So the iteration that advances the watermark counts as idle.

At one client, by the time the fsync completes the shard has already spun
past `spin_limit` waiting for it. So:

- iteration N: the fsync CQE arrives, the watermark advances, the conn
  re-queues itself for arming — and `io_work` is false, so the shard parks
  **before** the arming pass runs;
- nothing else can wake it: the only client is waiting for the reply that
  the un-run arming pass would have sent;
- the park's own timeout fires at 50 ms, iteration N+1 arms the conn, and
  the reply goes out.

At eight clients the other connections' `OP_RECV`/`OP_WRITE` completions set
`io_work`, the shard does not park in that window, and p99 falls to the
device's own figure — which is what the table shows.

The distinction the code draws is right and drawn one category too wide: a
park timeout and a waker read are administrative, but **an fsync completion
that moves the durable watermark is work with a pending consequence**.

## It is not only the concurrency harness, and it may be a regression

The serial harness sees the same thing. `bench/pgcompare.py`'s write shape is
one connection issuing one `HSET` at a time, and the fair 5.4 baseline,
median of three passes, puts `always` at:

| | write p50 | write p99 |
|---|---:|---:|
| kevy `always` | 2,889 | **47,264** |
| kevy `everysec` | 29 | 41 |
| PostgreSQL 18 | 825 | 1,678 |

Two harnesses, the same signature. And the number has moved: the
2026-07-26 record has this shape's p99 at **3,097 µs** — which is roughly
today's *p50*, i.e. that run had no tail of this kind at all.

Between the two, `48d06ae7` (2026-08-12) made **CQE-gated replies the
default for `always` on io_uring**; before it, `always` took the classic
synchronous path. If that is the cause, the 15× p99 difference is a
regression that shipped in 5.2 and 5.3 and nobody measured, because nothing
between those releases re-ran this shape.

That is checkable without touching code either: `KEVY_AOF_OFFLOAD=0` keeps
the classic path.

## The test, before any code

`park_timeout_ms` is an `[advanced]` config key and `KEVY_AOF_OFFLOAD` is an
environment switch, so both questions are answered by four runs that change
no code:

| run | what it decides |
|---|---|
| offload on, `park_timeout_ms` = 5 | if the p99 follows the setting, the mechanism above is the cause |
| offload on, 50 (default) | the reference |
| offload on, 200 | the same question from the other side — a p99 that does not move is a refutation |
| **offload off**, 50 | whether the classic path still has the 3,097 µs shape, which would make this a named regression |

That test changes no code, which is why it comes first. Only after it
decides is the fix worth writing — and the fix is then narrow: an OP_AOF
completion that advances the watermark counts as work for that iteration.

## Why it matters beyond the number

`always` is the mode where the published comparison has PostgreSQL ahead of
kevy (`docs/rds-workloads.md`: 3,097 µs against 1,689 µs at matched
durability). Part of that column is a scheduling artefact wearing a
durability number's clothes, and the doc says nothing about it.
