# v5 rc soak: 80 minutes, factory defaults — and the giant-collection boundary, measured

Pre-rc dogfood evidence (2026-08-10, box NVMe, default config: io_uring
+ AOF offload on, everysec, no tuning). Sustained moderate mixed load
(SET/GET/INCR/LPUSH/SADD/HSET, P4, c20, ~20s phases back-to-back),
60s health samples.

## The good

- **98 auto-rewrites completed autonomously** under live load — the
  begin-gate's other half proven: it defers under storms (tailgate)
  AND admits + converges under normal pressure.
- Steady-state reactor gap ≤ 47 ms across the run.
- Zero errors/panics in the server log, all samples.
- **Shutdown + restart at 41.6 GB used_memory / 1,000,003 keys:
  DBSIZE match, 150 s boot on a 79 GB data directory.** A recovery
  drill an order of magnitude past the gate suite's sizes.

## The boundary (known, now with numbers)

The benchmark's fixed-key collections (`mylist`, `myset`, `myhash`)
grow without bound, reaching multiple GB as SINGLE values. The gap
watermark told the story: 717 ms at minute 3, 7.4 s at minute 30,
9.5 s at minute 50 — stepping with collection size. Mechanism (named
in the S5-H probes, sub-threshold then, gigabyte-scale now): a write
to a collection whose Arc is pinned by a live rewrite/snapshot view
pays `Arc::make_mut`'s full deep clone ON THE REACTOR — seconds, once
the value is gigabytes.

Scope: requires BOTH a multi-GB single collection AND a write to it
during a rewrite window. Ordinary keyspaces (the tailgate cells, the
gate suite) never see it. Real-world exposure: unbounded queue/list
patterns at very large sizes.

Disposition for v5: documented operational boundary
(`docs/persistence.md` trade-offs, three languages). The deep fix —
element-granular COW or incremental serialization for giant
collections — is a designed post-v5 arc (parking lot; the same entry
as the v8 memory-experiment residual family).
