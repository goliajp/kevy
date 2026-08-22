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

## The test, before any code

`park_timeout_ms` is an `[advanced]` config key. If this mechanism is the
cause, **the p99 follows the setting**: at 5 ms the p99 lands near 5 ms, at
200 ms near 200. If it stays at 50 ms regardless, or does not move with it,
the mechanism is wrong and the code above is not the explanation.

That test changes no code, which is why it comes first. Only after it
decides is the fix worth writing — and the fix is then narrow: an OP_AOF
completion that advances the watermark counts as work for that iteration.

## Why it matters beyond the number

`always` is the mode where the published comparison has PostgreSQL ahead of
kevy (`docs/rds-workloads.md`: 3,097 µs against 1,689 µs at matched
durability). Part of that column is a scheduling artefact wearing a
durability number's clothes, and the doc says nothing about it.
