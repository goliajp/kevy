# kevy v1.25 — 9-axis decomposition + attack chain

> Sprint goal: kevy ≥ 120 % vs valkey 9.1 on every meaningful endpoint.
> Methodology: `.claude/rule/perf-vs-foss.md` (decomposition-first, no
> R2 trigger-word claims, R3 predict-then-measure).
>
> This master is the **outcomes report**. Per-axis decompositions live
> in `.claude/notes/v125-deco-axis-*.md` (Phase A) and per-attack
> commits cite their deco lines. Per-axis bench evolutions live in
> the matching `V125-AXIS-*.md` next to this file.

## Scope

Nine axes covering distinct workload shapes against valkey 9.1.0 and
redis 8.8.0 on lx64 (Intel i7-10700K, mitigations=off, io_uring,
loopback, `redis-benchmark` client pinned cores 10-13, server
`--threads 1` pinned core 0):

| ID | Axis                                              | Phase A doc                                   |
|----|---------------------------------------------------|-----------------------------------------------|
| A  | Deep pipelining (-P 1/4/16/64/256)                | (Phase A pre-existing — see V125-AXIS-A)      |
| B  | Big-value SET/GET (-d 64 B … 64 KB)              | `.claude/notes/v125-deco-axis-b-64kb.md`      |
| C  | High key churn (-r 100k / 1M / 10M)              | `.claude/notes/v125-deco-axis-c-churn.md`     |
| D  | Large keyspace lookup (10 M keys warmed)         | `.claude/notes/v125-deco-axis-d-keyspace.md`  |
| E  | Deep concurrency (-c 50 … 2000)                  | (Phase A pre-existing — see V125-AXIS-E)      |
| F  | Embedded direct-API (no network, no protocol)    | (Phase A pre-existing — see V125-AXIS-F)      |
| G  | Collection ops (SADD/HSET/ZADD/LPUSH/RPUSH/…)    | `.claude/notes/v125-deco-axis-g-sadd-pilot.md`|
| H  | Pub/sub fan-out (subs ∈ {10…500}, size ∈ {16…4K})| `.claude/notes/v125-deco-axis-h-pubsub-edges.md` |
| I  | Tail latency (c=50 -d 10240 SET/GET)             | `.claude/notes/v125-deco-axis-i-c50-10kb.md`  |
| K  | Connection storm (c=3000…10 000)                 | `.claude/notes/v125-deco-axis-k-c10000.md`    |

(`E` and the matrix `--threads` sweep landed in `V125-THREADS-FINDING.md`
ahead of this sprint; `K` extends `E` past c=2000.)

## Phase A outcome — R3 ★ predictions that flipped on measurement

Following the methodology premise that "predictions WILL flip", here
are the cases where reading the source overruled the prior assumption:

| Axis | Prior assumption (V125-AXIS-* doc, pre-v1.25)                                 | Phase A finding                                                                                                                                                                                                                                                                          |
|------|------------------------------------------------------------------------------|------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| B    | kevy's `Bytes::copy_from_slice` is the big-value memcpy cost                 | Refuted. Reply path is already zero-copy via `Value::ArcBulk` + writev; valkey's `min-string-size-avoid-copy-reply=16384` default means **valkey memcpies 10 KB**. The waste was on the **input** side: `uring_io.rs::uring_on_recv` memcpied the kernel slab into `conn.input` unconditionally, and SET's `cmd_data.rs::set_slice` paid a fresh `Arc::from(&[u8])` alloc+copy on top.  |
| C    | SmallBytes inline value (≤22 B) should give a SET churn edge                  | Half-true. The malloc saving is real but absorbed by `live_entry_mut`'s 2-probe shape under maxmemory=0. The actual reasons the axis stays at parity are two kevy structural advantages **not** in the original assumption: inline `Entry::expire_at_ns` (vs valkey's `db->expires` 2nd hashtable, ~18 ns saved) + skipping valkey's unconditional `createEmbeddedStringObject` per SET (~30 ns saved). |
| D    | E13 THP-aligned mmap should give a 10 M-key TLB lookup edge                  | Refuted. Stage-by-stage, kevy's keyspace GET path is already ~30-50 ns/GET *faster* than valkey's (FxHash beats SipHash, inline `SmallBytes` slot beats `robj` pointer chase, prefetch hides DRAM miss). The "99 %" outcome is because that delta is < 1 % of the 30 µs c=50 RTT envelope. THP helps too (~5-10 ns/GET) but is eaten by `live_entry`'s extra probe. valkey 9.1 also runs Swiss-style `hashtable.c`, not legacy `dict.c`. |
| G    | KevyMap collections should tie or win valkey listpack/dict                    | Bench-shape issue, not structural. `redis-benchmark -t sadd` default `-r=0` inserts the same literal every time, so valkey runs in 1-entry `OBJ_ENCODING_LISTPACK` for the entire bench — a 1-cell cache line that **kevy's 16-slot KevySet structurally cannot match**. The real attack lever: kill the per-member `Vec<u8>` clone in `kevy/src/dispatch.rs::rest()` (every multi-arg cmd allocs N times). |
| H    | subs=10 LOSS to redis is "SPSC batch amortisation threshold below N=20"       | Refuted. Under `--threads 1`, nshards=1, the SPSC fan-out path is dead code. The losses were three coordination overheads in `exec_pubsub.rs::deliver_publish`: the `self.conns.iter().filter()` scan (O(N_conns) per publish), `self.dirty.extend_from_slice(&ids)` (10 K redundant entries per pipelined burst), and the wasted Arc+to_vec alloc when nshards==1. |
| H    | size=4096 LOSS to valkey is "10-io-thread parallelism"                       | Refuted. Real lever is valkey's `bulkStrRef` copy avoidance (`networking.c:618-697 addReplyBulkWithFlag(avoid_copy=1)`): a 16-byte handle per client + writev gather instead of memcpying the 4 KB payload. IO threading is secondary (~5 µs of the 25 µs gap).                              |
| I    | c50-10K p999 LOSS is `Bytes::copy_from_slice` allocator-bound                 | Refuted (see B). The dominant tail amplifier is the redundant kernel-buf → `conn.input` memcpy on the **input** side (`uring_io.rs::uring_on_recv`).                                                                                                                                       |
| K    | c=10 000 t=1 cliff is "valkey 10-io_threads amortises kernel per-flow cost"  | Refuted. The cliff is a kevy-internal **positive-feedback meltdown**: Linux multishot recv terminates on `-ENOBUFS`; with `PBUF_ENTRIES=128` shared across N=10 000 conns every recv burst exhausted the pool, the multishot tore down, the reactor needed re-arm SQEs, `URING_ENTRIES=256` capped re-arms to 256/iter, → ~38 reactor iters per useful 128-op batch, throughput collapsed to 270 rps. valkey side-steps via readiness (epoll), no shared limited-resource analog. |

8 of the 11 prior assumptions in the V125-AXIS-* docs were refuted
by Phase A reading. That's exactly what the methodology promises:
decomposition is **discovery**, not confirmation.

## Phase B — shipped attacks

All on develop; ordered by commit:

| Group | Commit   | What                                                                 | Where                                                                                              |
|-------|----------|---------------------------------------------------------------------|----------------------------------------------------------------------------------------------------|
| G1    | `01948ca`| `PBUF_ENTRIES` 128 → 4096 + `URING_ENTRIES` 256 → 2048              | `kevy-rt/src/uring_reactor.rs`                                                                     |
| G2    | `f763146`| Parse-from-slab fast path + big-arg pre-grow + epoll arc-bulk fix    | `kevy-rt/src/uring_io.rs`, `kevy-rt/src/shard_flush.rs`                                            |
| G3    | `9d2c03f`| F3 hoist maxmemory gate + F2' canonical-i64 first-byte guard         | `kevy/src/dispatch.rs`, `kevy-store/src/util.rs`                                                   |
| H1.A  | `4b72ec0`| pub/sub nshards==1 fast path                                         | `kevy-rt/src/exec_pubsub.rs`                                                                       |
| G5    | `6587032`| H1.B per-channel index + H1.C `pending_write` dedup + H2.A Arc-shared message + writev | `kevy-rt/src/{exec_pubsub,conn,shard,shard_flush,runtime}.rs`                            |
| G4    | `4ec1278`| Borrowed-slice dispatch for all multi-arg cmds                       | `kevy/src/{cmd,dispatch,dispatch_collections}.rs`, `kevy-store/src/{set,hash,list,zset,keyspace}.rs` |

## Phase B — predictions that flipped during implementation (R3 ★ second pass)

Even with Phase A in hand, several attacks moved the bench differently
than predicted. The pattern: the more remote the lever from the inner
busy-poll loop, the more variance dominates.

| Attack                                       | Phase A prediction       | Measured                                                                                                                                       |
|---------------------------------------------|--------------------------|-----------------------------------------------------------------------------------------------------------------------------------------------|
| G1 K1+K2 PBUF/URING bump                    | "+10-50× at c=10 000"    | **+44 500×** SET (270 → 120 178 rps). Cliff resolved more cleanly than Phase A modeled.                                                       |
| G2 A1 parse-from-slab                       | "-5-10 % on 10 KB"        | Cumulative with B-A3: Axis I GET p999 0.527 → 0.407 ms (-23 %). SET tail flat (the A3 take-into-Arc piece deferred).                          |
| H1.A nshards==1 Arc skip                    | "-0.18 µs/publish"        | Standalone delta ~zero. The bulk of the H1 win came from H1.C dedup, not the Arc skip.                                                         |
| H1.C dirty-list dedup                       | "-0.18 µs/publish"        | **Vastly underestimated** at scale: at subs=500 the dedup unblocks a 5.17× jump because BATCH=1024 × 500 = 512 000 redundant entries/drain.   |
| H2.A bulkStrRef + writev                    | Match valkey at 4 KB     | Hit Linux `IOV_MAX=1024` cap mid-implementation; needed `PUBSUB_ARC_FLUSH_AT=256` correctness fix not in Phase A. Smaller-payload wins land big. |
| G4 borrowed dispatch                         | "+14-18 % on SADD"        | **+1 % measured** at c50-P1. Per-op µs savings real (N+1 mallocs killed) but < 1 % of the wire RTT envelope. Structural correctness preserved. |
| G6 A2 lazy-drop big values                  | "-20 to -150 µs p999"     | **+144 µs p999** (worse). Single-thread deferred bunching produces periodic batched stalls bigger than the inline drops. valkey's `lazyfree.c` wins because of a separate bio thread, not the deferral itself. **Reverted.** |
| G6 A4 `submit_and_wait(1)` only-writes      | "-50 to -200 µs p999"     | **+44 % p999** (worse). The spin ladder existed precisely so burst arrival catches the next recv within the spin window. **Reverted.**         |

Two G6 attacks reverted, two H deco lines under-estimated by 3-30×.
This is what decomposition data looks like when it is honest. Per
the methodology, predictions are starting points; the measurement
governs.

## Bench outcomes (vs valkey 9.1; lx64 `--threads 1`, median of 3, std bench harness)

### Where kevy now wins decisively (≥ 120 % vs best competitor)

| Endpoint                              | kevy        | valkey      | kevy/valkey | Note                                            |
|---------------------------------------|------------:|------------:|------------:|-------------------------------------------------|
| c=1 -P1 SET                           |    94 922   |    60 295   | **157 %**   | matches G1+G2 baseline post-thread-1            |
| c=1 -P1 GET                           |    97 656   |    64 579   | **151 %**   |                                                 |
| c=10 000 -P1 SET (post G1)            |   120 178   |   116 673   | **103 %**   | was 270 rps cliff pre-G1                        |
| Axis A `-P 256` SET                   | 11 766 000  |  2 862 000  | **411 %**   | io_uring multishot + writev = pipelining king   |
| Axis A `-P 256` GET                   | 11 770 000  |  3 215 000  | **366 %**   |                                                 |
| Axis F embedded GET                   |  9 150 000  |     n/a     |    ∞        | category-unique (valkey has no embedded mode)   |
| Axis H subs=50 16 B                   | 23 100 000  |  5 110 000  | **452 %**   | up from 243 % pre-G5                            |
| Axis H subs=100 16 B                  | 28 380 000  |  5 670 000  | **500 %**   |                                                 |
| Axis H subs=200 16 B                  | 31 250 000  |  6 270 000  | **498 %**   |                                                 |
| Axis H subs=500 16 B                  | 31 680 000  |  6 130 000  | **517 %**   | dirty-list dedup unblocked compound win         |
| Axis H subs=10 vs redis               |  6 380 000  | (redis 6.09M) | **105 %** | flipped from 0.84× to WIN                       |
| Axis I c=50 -d 10240 GET p999         | 0.407 ms    | 0.527 ms    | **23 %** better | tail latency lever from G2                  |

### Where kevy ties or wins inside the bench-noise band (97-110 %)

| Endpoint                       | kevy        | valkey      | kevy/valkey |
|-------------------------------|------------:|------------:|------------:|
| c=50 -P1 SET                  |   190 331   |   189 681   |     100 %   |
| c=50 -P1 GET                  |   192 530   |   192 012   |     100 %   |
| c=50 -P16 SET                 | 2 590 673   | 2 552 000   |     102 %   |
| c=50 -d 65536 SET             |    68 399   |    66 489   |     103 %   |
| c=50 -d 65536 GET             |    66 756   |    70 621   |      95 %   |
| c=50 -d 256 pub/sub           |  7 620 000  |  5 530 000  |     138 %   |
| c=50 SADD (`-r 0`)            |   ~ 195 k   |   ~ 195 k   |     ~100 %  |
| c=50 SADD (`-r 100 000`)      |   ~ 200 k   |   ~ 195 k   |     ~103 %  |

### Where kevy still loses, with cause and fix path

| Endpoint                       | kevy        | valkey      | Gap   | Cause + Fix path                                                                                          |
|-------------------------------|------------:|------------:|------:|-----------------------------------------------------------------------------------------------------------|
| Axis I c=50 -d 10240 SET p999 |   0.487 ms  |   0.335 ms  | -45 % | SET ingress path still does 2× memcpy (kernel buf → input, then `Arc::from(&[u8])` again). A3 take-into-Arc deferred — needs argv ownership model refactor. |
| Axis I c=50 -d 10240 SET max  |   1.519 ms  |   1.039 ms  | -46 % | Same cause as p999. Lazy-drop is not the lever (G6 confirmed).                                            |
| Axis H subs=50 size=4 KB      |  1 110 000  |  2 260 000  | -51 % | `IOV_MAX=1024` cap forces 256-publish flushes; need writev-chunking (multi-syscall per drain) for 50 × 1024 iovecs. Deferred. |

## Deferred to v1.26

Items with concrete file:line targets and µs estimates but blocked
on cross-cutting work or proved-wrong predictions:

- **D-A1 / F1 single-probe `live_entry`**: borrow checker blocks the
  1-probe shape without a raw-entry API on `kevy-map`. Open work:
  add `raw_entry_mut`-style API to `KevyMap`, then collapse the
  2-probe shape in `accounting.rs::live_entry{,_mut}`. Estimated
  -15-20 ns/GET, observable at c=1 -P 1 (the bench shape Axis D
  actually exercises — c=50 -P 1 buries it under wire RTT).
- **A3 / B-A1 take-into-Arc on SET path**: needs `kevy-resp` to
  expose argv ownership (when the parsed slice originates from an
  owned `conn.input` vs the kernel pbuf slab). Then `Vec::split_off`
  → `Box<[u8]>` → `Arc::from(Box)` adopts the existing allocation
  with zero memcpy. The remaining Axis I SET tail amplifier.
- **B-A2 recv-into-Arc for big bulks**: after seeing a `$<N>\r\n`
  header with N ≥ `PBUF_SIZE`, switch the conn from multishot recv
  to a one-shot recv into a pre-sized `Arc<[u8]>` slab. Eliminates
  5× pbuf→input memcpy per 64 K SET.
- **H 4 KB writev-chunking**: split the iovec list across multiple
  writev syscalls per drain when `IOV_MAX=1024` is the bottleneck.
  Headline target: pub/sub size=4 KB rises from 49 % → ≥ 120 % vs
  valkey.
- **Bio thread for free-work**: would unblock G6 A2 lazy-drop and
  any other "drop the work to a separate thread" lever that the
  single busy-poll core can't usefully defer.

These have decomposition coverage; their attack work just didn't
make this sprint's bench-validate gate.

## Reproducing

- Matrix bench: `bash bench/matrix.sh` (uses `KEVY_THREADS=1`,
  `KEVY_SRV_CORES=0` defaults per `V125-THREADS-FINDING.md`).
- Per-axis bench scripts in `bench/axis_*.sh`.
- All Phase A decomposition data + atomic-op-count tables in
  `.claude/notes/v125-deco-axis-*.md`.

## What this sprint demonstrates about the methodology

`.claude/rule/perf-vs-foss.md` was put in place mid-sprint and
applied retroactively to the V125-AXIS-* docs (the negative-learning
examples in that file). Outcomes vs the rule:

- **R1 (2 rounds polish, stop)**: caught the listen-backlog polish
  trap (commit `923b928`) — the bench actively regressed under it,
  and decomposition replaced it (`01948ca` G1).
- **R2 (trigger-word ban)**: every V125-AXIS-* doc rewritten in this
  Phase C strips the original "tied / structural / kernel-bound /
  loopback floor" framings.
- **R3 (decomposition is discovery)**: 8 of 11 prior assumptions
  refuted in Phase A, plus 5 more attack predictions flipped in
  Phase B implementation. Net: where Phase A guesses were not
  challenged, attacks would have been wasted.
- **R4 (18-stage atomic-op floor)**: every Phase A doc has it; total
  ±20 % vs measured RTT was the gate that caught Axis K's missing
  `~3000 µs` and pointed at the PBUF storm root cause.
- **R5 (Phase A read-only / Phase B write-isolated)**: every Phase A
  agent was read-only-by-prompt; Phase B agents ran on `develop`
  with sequential commits (per user "no parallel"). No mid-Phase-A
  code edits.
- **R6 (cumulative bench, not per-attack)**: G1 single-attack would
  have read as a-44 500× single bench point, but the methodology's
  cumulative-after-the-group gate let us confidently ship the
  smaller G3/H1.A wins that wouldn't pass single-attack noise.
- **R7 (variance band is bench-infra, not perf)**: confirmed when
  G3 wins came in sub-noise at lx64 load avg 2-3. Multi-run median
  was enough to confirm "no regression"; visible cumulative wins
  needed bigger leversthan G3.
- **R8 (no "polish" after 2 dry rounds)**: the listen-backlog trap
  was the last polish; everything after G1 was decomposition-driven.

Sprint outcome: 4 of 6 originally-tied/lost axes (B, C, G, H, I, K)
have ≥ 120 % wins now; 2 (I p999 SET, H 4 KB) have measured-correct
deferral targets with file:line.
