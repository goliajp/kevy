# Axis F — embedded (kevy-embedded direct calls)

**Hypothesis**: kevy-embedded is a unique capability — valkey-server
is a TCP-only daemon with no in-process mode, so this axis is
**kevy-unique** rather than side-by-side. Goal: quantify the
in-process ceiling so the network ones can be put in perspective.

## Methodology

- `cargo run -p kevy-embedded --example axis_f_embedded --release`
- N = 1 000 000 ops per scenario (10 KB-value scenarios use N/10
  to keep wall-clock comparable).
- Single-threaded — kevy-embedded is the in-process keyspace, no
  reactor, no protocol.
- lx64, same host as the TCP matrix (cores 10 pinned via `taskset`).

## Result (kevye only — no competitor has embedded mode)

| op                    | ops/sec       | ns/op |
|-----------------------|---------------|-------|
| **GET (miss)**        |  **38 780 899** |    26 |
| **GET 1 B (hit)**     |   **9 690 939** |   103 |
| **GET 16 B (hit)**    |   **9 151 171** |   109 |
| **INCR (L2 Int fast)**|   **7 973 203** |   125 |
| **SET 16 B**          |   **7 076 741** |   141 |
| **SET 1 B**           |   **6 788 282** |   147 |
| **GET 1 KB**          |   **4 914 549** |   203 |
| **SET 1 KB**          |   **4 038 232** |   248 |
| **DEL**               |   **1 896 058** |   527 |
| **GET 10 KB**         |   **1 399 908** |   714 |
| **SET 10 KB**         |   **1 219 795** |   820 |

## Interpretation

This is the **userspace data-path ceiling for kevy** — no network,
no protocol, no kernel netstack. Compare against the TCP-matrix
numbers:

| op shape         | kevye (this axis) | kevy TCP (matrix) | network tax |
|------------------|-------------------|-------------------|-------------|
| GET 16 B         | 9 151 171         | 83 705 (c1-P1)    | **109×**    |
| SET 16 B         | 7 076 741         | 82 418 (c1-P1)    | **86×**     |
| INCR             | 7 973 203         | 195 427 (c50)     | 41× (but
                                                                  c50
                                                                  amortises)
| GET 10 KB        | 1 399 908         | 157 356 (c50)     | 9× (network
                                                                  bandwidth
                                                                  dominates)

**That's how much wire + kernel + protocol cost kevy** —
between 9× (big values, kernel-bandwidth-bound) and 109× (small
values, RTT-bound) the per-op overhead.

For comparison: valkey has **no embedded mode at all**. The
nearest equivalent is `redis-cli ... > /dev/null` over UNIX socket
which still goes through the full RESP protocol parse + reply
encode. **kevy-embedded is a category that valkey/redis
literally cannot enter.**

## What this axis confirms about composed wins

- **L2 INT fast path works**: INCR 7.97 M ops/s @ 125 ns/op is
  near GET hit's 109 ns/op — the only cost above GET is the
  i64 addition + the in-place write. No parse, no format, no
  alloc.
- **SmallBytes inline win is visible**: SET 1 B / SET 16 B
  cluster at 6.8-7.1 M ops/s (same order, both inline). The 24 B
  bucket layout absorbs both.
- **ArcBulk threshold split is visible**: SET 1 KB 4.0 M (Arc
  alloc per op); SET 10 KB 1.2 M (page-fault cost per Arc
  page; 10 KB straddles two 4 KB pages).

## Honest verdict

✅ **Unique kevy capability** — no competitor comparison applies,
but the raw numbers (9 M GET/s, 8 M INCR/s, 38 M GET-miss/s)
contextualise the TCP-matrix gaps as "network/kernel tax" vs
"kevy's actual data-path speed".

## Use cases for kevy-embedded

- **Application-private cache** (no IPC for the embed app)
- **Sidecar tooling** (in-process state without spawning a daemon)
- **WASM blobs** (kevy-embedded compiles to wasm32, valkey-server
  doesn't)
- **Test fixtures** (drop-in Store for unit tests)
- **Production embed mode** (already shipping — see mailrs
  dogfood from prior session)

## Reproduce

```bash
cargo run -p kevy-embedded --example axis_f_embedded --release
# or on lx64:
ssh lx64 'cd /root/kevy && cargo run -p kevy-embedded --example axis_f_embedded --release'
```

## Status

✅ **CONFIRMED unique capability.** kevye sits 9-109× the kevy-TCP
numbers — exposing the network/kernel tax. valkey + redis cannot
participate in this axis at all.
