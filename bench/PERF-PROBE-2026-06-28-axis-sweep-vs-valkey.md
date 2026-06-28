# Axis-sweep probe vs valkey 9.1.0 — 2026-06-28 (lx64)

Following [PERF-DECOMP-2026-06-28-c100-GET-vs-valkey-9.1.md](PERF-DECOMP-2026-06-28-c100-GET-vs-valkey-9.1.md) §A0 — kevy was found to **already lead valkey 9.1 at c1/c100 GET** (2.98× / 1.40×). The natural next question, per project North Star ("对标并远超 valkey 9.1; 目标 = 硬件 ceiling, 不是 valkey ceiling"): **at which workload does kevy actually LOSE?**

This probe answered that. Three axes swept; **kevy loses at large payloads**. Specific gaps below.

## Bench setup

- Host: lx64 16-core (mitigations=off from session 8), kernel 6.12
- kevy: `/root/kevy/target/release/kevy` @ workspace v1.24.0 (functionally equivalent to current develop = v1.28.0 for the GET / SET / PUB hot paths — Lua sprint + workflow fix don't touch these). `--threads 2` for axis A/B, `--threads 1` for axis H. `taskset -c 0-1` (axis A/B) / `-c 0` (axis H).
- valkey 9.1.0: `/root/srcbench/valkey/src/valkey-server` `--io-threads 10 --io-threads-do-reads yes` `taskset -c 0-9`. Note: **kevy was handicapped to 1-2 cores vs valkey's 10 cores** — the comparison is "thin kevy vs fat valkey" intentionally.
- redis 8.8: `/root/srcbench/redis/src/redis-server`, same `taskset -c 0-9 --io-threads 10`
- redis-benchmark 8.0.2, taskset `-c 10-13`, RUNS=3 medianed per cell

## Axis A — pipelining sweep (`-P 1 / 4 / 16 / 64 / 256`)

`bench/axis_a_pipeline.sh`, `-c 50 -t set,get`, N=500k.

| -P | op | kevy(2c) | redis(10c) | valkey(10c) | kevy / valkey |
|---|---|---|---|---|---|
| 1 | GET | 199,203 | 165,837 | 193,199 | 1.03× |
| 1 | SET | 198,728 | 162,866 | 194,553 | 1.02× |
| 4 | GET | 765,697 | 598,802 | 744,048 | 1.03× |
| 4 | SET | 775,194 | 603,136 | 747,384 | 1.04× |
| 16 | GET | 2,688,172 | 2,173,913 | 2,475,248 | 1.09× |
| 16 | SET | 2,645,503 | 2,083,333 | 2,057,613 | **1.29×** |
| 64 | GET | **7,353,412** | 3,205,333 | 2,525,414 | **2.91×** |
| 64 | SET | **7,353,412** | 2,347,568 | 1,845,136 | **3.98×** |
| 256 | GET | **11,910,095** | 3,186,140 | 2,908,279 | **4.09×** |
| 256 | SET | **10,643,064** | 2,348,470 | 1,880,541 | **5.66×** |

**Verdict**: NOT a losing axis. kevy wins at every -P, and the gap widens as -P grows. The script's hypothesis ("io_uring multishot recv + writev means kevy per-cmd kernel cost stays flat as -P grows") is confirmed: at -P=64+ valkey saturates its per-request kernel cost while kevy keeps amortizing across the deeper queue. **2 cores of kevy outscale 10 cores of valkey** above -P=16.

## Axis B — big value sweep (`-d 64 / 256 / 1K / 4K / 16K / 64K`)

`bench/axis_b_bigval.sh`, `-c 50 -t set,get`, N=200k.

| -d (B) | op | kevy(2c) | redis(10c) | valkey(10c) | kevy / valkey |
|---|---|---|---|---|---|
| 64 | GET | 198,741 | 157,729 | 190,537 | 1.04× |
| 64 | SET | 194,049 | 162,602 | 188,679 | 1.03× |
| 256 | GET | 198,478 | 163,979 | 193,174 | 1.03× |
| 256 | SET | 198,413 | 162,866 | 191,693 | 1.04× |
| 1,024 | GET | 192,864 | 160,966 | 190,840 | 1.01× |
| 1,024 | SET | 193,330 | 155,219 | 189,663 | 1.02× |
| 4,096 | GET | 178,571 | 142,349 | 172,265 | 1.04× |
| 4,096 | SET | 171,380 | 137,174 | 170,213 | 1.01× |
| 16,384 | GET | 142,450 | 119,332 | 139,665 | 1.02× |
| 16,384 | SET | 137,363 | 118,906 | 137,931 | 1.00× (tie) |
| 65,536 | GET | 69,832 | 62,035 | **71,839** | **0.97× LOSS** |
| 65,536 | SET | 63,613 | 61,200 | **69,252** | **0.92× LOSS** |

**Verdict**: kevy wins 64B-16KB. **At 64KB, kevy LOSES** — GET by 3%, SET by 8%. The 16KB → 64KB transition is where kevy's value-write path stops scaling proportionally.

## Axis H — pub/sub fan-out

`bench/axis_h_pubsub.sh` via `kevy-pubsub-bench`, kevy `--threads 1` taskset `-c 0`.

| subs | msgs | size (B) | kevy | redis | valkey | kevy / valkey |
|---|---|---|---|---|---|---|
| 10 | 100,000 | 16 | 6.64M | 6.15M | 4.11M | 1.61× |
| 50 | 100,000 | 16 | 27.65M | 11.17M | 5.80M | **4.77×** |
| 100 | 50,000 | 16 | 38.03M | 12.01M | 5.70M | **6.67×** |
| 200 | 20,000 | 16 | 42.79M | 11.47M | 6.10M | **7.02×** |
| 500 | 10,000 | 16 | 36.16M | 10.67M | 6.00M | **6.03×** |
| 50 | 50,000 | 256 | 7.99M | 6.52M | 5.30M | 1.51× |
| 50 | 20,000 | 4,096 | 1.64M | 0.75M | **1.68M** | **0.97× LOSS** |

**Verdict**: kevy wins decisively at small payloads (4-7× over valkey at fan-out > 50 subs, 16B msg). **At 4KB msg, kevy LOSES to valkey by 3%**. Same large-payload symptom as axis B.

## Cross-axis finding

**Axes A and H both show: kevy loses at large payloads.**

| Axis | Workload | kevy result | Common factor |
|---|---|---|---|
| B | -d 65536 GET/SET | -3% / -8% LOSS | 64KB value memcpy/writev/buffer growth |
| H | size=4096 pubsub | -3% LOSS | 4KB per-sub broadcast write path |
| A | -P 16-256 | wins 1.1-5.7× | pure command throughput, small payloads |

The losing axis is consistently **"single large bytes leaving the server"**. The winning axis is **"many small bytes leaving the server"**.

Three hypotheses for the large-payload loss (to be tested in a future Phase A decomp):

1. **`Conn::output` buffer growth strategy** — kevy may double-grow its per-conn output buffer for big writes, doing an extra memcpy on every doubling that valkey avoids via direct iovec.
2. **io_uring iovec chain assembly cost** — kevy assembles iovec entries for writev per-conn per-iter. At 64KB payloads, the iovec setup cost is amortized across fewer entries; the small-buffer optimization kevy uses for small replies inverts at large sizes.
3. **TCP loopback MSS interaction** — at 64KB writes, the kernel splits the payload across multiple loopback packets. Maybe kevy's flush triggers a separate `io_uring_enter` per packet boundary while valkey's writev makes one big send.

valkey's relevant code paths for comparison:
- `tryAvoidBulkStrCopyToReply` in `networking.c` — directly writev the value buffer
- `_addReplyToBufferOrList` chain in `networking.c` — fallback buffer building

kevy's relevant code paths:
- `kevy-rt/src/shard_flush.rs::flush_backlog` — write-side dispatch
- `kevy-rt/src/conn.rs` + `kevy-resp/src/encode.rs` — buffer assembly
- `kevy-uring/src/io.rs` — io_uring write SQE submission

## What this probe settles

1. **kevy is NOT behind valkey 9.1 at any small-payload workload tested** (c1/c100 GET, deep pipeline, large fan-out small msg).
2. **kevy IS behind valkey 9.1 at large-payload workloads** — 64KB values (-3% to -8%) and 4KB pub/sub messages (-3%).
3. The losing workload has a clear pattern: **single large write per request**, not "many small writes". This is the Phase-A target for the next sprint.

## What this probe does NOT settle

- **vs tuned valkey at small payloads** — valkey here ran `--io-threads 10 --io-threads-do-reads yes` on 10 cores, which is fair tuning. kevy ran on 2 cores. So at small payloads, "kevy 2-core ≈ valkey 10-core" is the apples-to-apples result; "kevy 10-core" likely keeps the lead but unmeasured.
- **Why specifically at 16KB→64KB the inversion happens** — needs perf record + flame graph at -d 65536 to localize. Future Phase A.
- **Whether the 64KB loss is the redis-benchmark wire / TCP loopback limit being hit, not kevy/valkey limits** — at -c 50 -d 65536 = 3.2 MB in-flight per round; if loopback bandwidth saturates somewhere, both servers should plateau. The fact that kevy plateaus lower than valkey says it's a server-side issue, not a wire one.

## Next-session entry points (recommendation order)

1. **Phase A decomp of -d 65536 SET** — 18-stage side-by-side kevy vs valkey 9.1 source, focused on the write path from `Conn::output` setup → io_uring write SQE → CQE handling. Read `tryAvoidBulkStrCopyToReply` carefully — valkey's specific big-value optimization. Output: `bench/PERF-DECOMP-<date>-bigval-vs-valkey.md`.
2. **Phase B attack** based on whichever hypothesis above is confirmed by perf record.
3. **Validation** — re-run axis B at -d 65536 to confirm the attack closed the 8% gap.

Do NOT continue userspace polish on the c100 small-GET hot path (the Phase A decomp from earlier in this session enumerated 14 attacks there with ~5-10% combined ceiling, but kevy already leads valkey there — opportunistic, not necessary).
