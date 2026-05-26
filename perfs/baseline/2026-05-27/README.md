# Baseline v0.1.0 — pre-deep-polish snapshot (2026-05-27)

This is the **reference point** for kevy's stone-deep-polish work
([[feedback-mailrs-stone-deep-polish-method]]). Every subsequent stone
version's perf snapshot will be compared against the corresponding numbers
in this directory. E2E re-bench (Phase E) compares its post-polish
numbers against `e2e-mac-aarch64.log` here.

> Methodology: capture **first**, then optimize. No stone polish is
> allowed to land without a prior baseline snapshot it can be diffed
> against.

## Environment

- **Host**: macOS 26.5 (build 25F71), Apple M4 Pro, arm64
- **Toolchain**: rustc 1.95.0 (59807616e 2026-04-14)
- **Git sha**: `167bb5b` (`chore(sys): drop orphaned uring.rs from kevy-sys`)
- **e2e harness**: `bash bench/run.sh`, Docker Desktop, isolated CPU
  pinning (servers cores 0–9, loadgen 10–13)
- **Stone harness**: `cargo run -p <stone> --example <bench> --release`
  (Rust 1.95, fat LTO, codegen-units=1)

> **Caveat — mac docker plane vs lx64 metal plane**: absolute RPS on the
> macOS Docker VM is depressed relative to a Linux metal box (see
> `bench/REPORT.md`). The numbers here are the **mac aarch64 docker
> plane** of the baseline. The companion **lx64 metal plane** is captured
> separately by the user's metal harness (see `perfs/data/2026-05-26/
> metal-*` for prior lx64 numbers). Phase E re-bench compares **same
> plane** to **same plane** — never mix mac docker numbers with lx64
> metal numbers.

## E2E baseline (mac aarch64 docker plane)

`bench/run.sh`, n=200000, no pipelining, tests=ping/set/get/incr.

| test          | valkey 9.1 -c 1 (rps) | kevy v0.1.0 -c 1 (rps) | kevy / valkey | valkey -c 50 (rps) | kevy -c 50 (rps) | kevy / valkey |
|---------------|----------------------:|-----------------------:|--------------:|-------------------:|-----------------:|--------------:|
| PING_INLINE   | 9,932                 | 5,597                  | 0.56×         | 117,540            | 74,397           | 0.63×         |
| PING_MBULK    | 10,270                | 6,124                  | 0.60×         | 127,558            | 69,145           | 0.54×         |
| SET           | 11,549                | 6,567                  | 0.57×         | 116,878            | 66,305           | 0.57×         |
| GET           | 9,627                 | **616**                | **0.06×**     | 125,727            | 50,219           | 0.40×         |
| INCR          | 9,508                 | **618**                | **0.06×**     | 128,168            | 38,027           | 0.30×         |

**Observation (recorded, not acted on in Phase B)**: kevy GET/INCR -c1
shows ~616 rps vs ~6,000 rps for SET — a 10× internal drop. This does
NOT match the `[[project-kevy-roadmap-state]]` lx64 metal numbers
(c1 1.1–1.3× **lead** vs valkey). Two non-exclusive hypotheses:

1. mac docker VM amplifies a specific path (likely lazy-expiry per-op
   `Instant::now()` syscall) much more than lx64 metal does, because
   mac docker syscall cost is higher.
2. A regression slipped in between the `[[project-kevy-roadmap-state]]`
   v0.metal snapshot and current `167bb5b`. Both deserve investigation
   in Phase E (post stone polish) when we re-bench; if c1 GET/INCR still
   show this gap on lx64 metal in Phase E, it's a real regression and
   Phase E hot has to widen to cover the cement path.

For Phase B (baseline-only), we record the number as-is.

## Per-stone baseline snapshots

Each stone's micro-bench output, captured pre-deep-polish.

| stone              | status                              | file                                              |
|--------------------|-------------------------------------|---------------------------------------------------|
| kevy-bytes         | ✅ captured                          | [`kevy-bytes-v0.1.0.txt`](kevy-bytes-v0.1.0.txt) |
| kevy-hash          | ✅ captured (timer resolution issue) | [`kevy-hash-v0.1.0.txt`](kevy-hash-v0.1.0.txt)   |
| kevy-ring          | ✅ captured                          | [`kevy-ring-v0.1.0.txt`](kevy-ring-v0.1.0.txt)   |
| kevy-resp          | ✅ captured                          | [`kevy-resp-v0.1.0.txt`](kevy-resp-v0.1.0.txt)   |
| kevy-map           | ✅ captured (vs std+FxHash)          | [`kevy-map-v0.1.0.txt`](kevy-map-v0.1.0.txt)     |
| kevy-madvise       | ⏸ deferred — no bench example yet (Phase P3 will add); function is one syscall + integer math, perf bench is post-polish work | — |
| kevy-resp-client   | ⏸ deferred — no bench example yet (Phase P6 will add); requires loopback fixture | — |
| kevy-uring         | ⏸ deferred — Linux-only crate; mac arm64 path is `#![cfg(target_os = "linux")]` empty (Phase P8 requires lx64 metal harness) | — |
| kevy-bench         | n/a — dev-tool harness; not bench'd against itself | — |

### Headline stone numbers (mac aarch64, release profile, M4 Pro)

Pulled from the snapshot files. Numbers are pre-deep-polish; Phase P
sprints will compare against these.

- **kevy-bytes** (vs `Vec<u8>`): inline-SSO len/copy at sub-ns scale —
  bench harness timer resolution insufficient at this size, needs
  longer iteration counts in P1.
- **kevy-hash** (vs FxHasher / SipHash): u64 keys at sub-ns — same timer
  resolution issue at u64 width; longer-key bench has real numbers in
  the snapshot.
- **kevy-ring**:
  - push+pop u64 same-thread: median 2 ns / p95 3 ns
  - cross-thread SPSC cap=256: 7.7M items/s, 129.9 ns/item
  - cross-thread SPSC cap=1024: 13.8M items/s, 72.7 ns/item
- **kevy-resp** (encode):
  - bulk: median 7 ns / p95 12 ns
  - simple string: median 4 ns / p95 16 ns
  - integer: median 6 ns / p95 16 ns
- **kevy-map** (vs `std::HashMap` + FxBuildHasher):
  - 256 keys: insert 0.85×, get-hit 1.00×
  - 4,096 keys: insert 0.92×, get-hit 1.40× ← already lead `std+Fx` on cached gets
  - 65,536 keys: insert 0.95×, get-hit 0.87× ← regresses on cache-miss-dominated gets
  - **kevy-map's P7 mission**: push 65k get-hit ≥ max(hashbrown, std+Fx,
    absl::flat_hash_map, tsl::robin_map, boost::unordered_flat_map,
    khash, Go runtime/map).

## What this baseline is NOT

- **Not a perf claim.** kevy v0.1.0 GET/INCR -c1 in mac docker is bad.
  We're recording it. We are not defending it.
- **Not the lx64 metal plane.** That plane has different absolute
  numbers and likely a different ratio. See `[[feedback-kevy-bench-
  isolation]]` for why mac docker is starvation-biased on busy-poll
  servers.
- **Not the post-polish target.** P1..P8 will each push above
  `max(competitor)` on the stone's primary metric. The e2e numbers may
  or may not move (Phase E checks transmission).

## Next phase

Phase T (tool baseline) — install hyperfine, cargo-llvm-cov, dhat-rs,
cargo-fuzz, miri nightly, loom. Then Phase R (refactor large files/fns).
Then Phase P1 (kevy-bytes deep-polish).
