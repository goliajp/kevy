# v1.deep-polish — close (2026-05-27)

Closes the v1.deep-polish version per
[[feedback-mailrs-stone-deep-polish-method]]: 8 stones, each polished
on the 5 dimensions (perf / mem / size / doc / test), measured against
cross-language competitors (Rust + C + C++ + Go), versioned snapshot
captured for the v0.1.0 release.

## What landed (commit timeline, branch `develop`)

| commit | phase | what |
|---|---|---|
| `167bb5b` | (cleanup) | drop orphaned `uring.rs` left by the v0.polish split |
| `44002d1` | **B** baseline | pre-polish e2e mac docker + 5 stone snapshots |
| `3c73bf7` | **T** tools  | hyperfine / llvm-cov / dhat / cargo-fuzz / miri ready; perfs/comparative/ skeleton |
| `c297a17` | **R** refactor | split kevy-resp/uring/map src files + 4 fns into proper mods, all stone files ≤ 500 prod LOC |
| `5d869de` → `076a740` | **P1** kevy-bytes | Clone + PartialEq + alloc_heap specialised; clone_heap 36 → 19 ns (35%); cov 70 → 98.74%; miri 30/30 |
| `aa1e435`, `436566d` | **P2** kevy-hash | Rust + C (xxh3, wyhash) + Go (maphash) + C++ (std::hash) competitor cohort; cov 93 → 100%; miri 10/10 |
| `6ac1e03` | **P3** kevy-madvise | cov 100% already; BASELINE doc |
| `51b1c9b` | **P4** kevy-ring | **cached SPSC cursors — cross-thread 52 → 4 ns (13× faster), now leads rtrb/ringbuf/crossbeam**; miri 7/7 post-change |
| `e54b261` | **P5** kevy-resp | redis-rs cohort: kevy 9× faster than redis-rs on parse_command + parse_reply; cov 91 → 97% |
| `85dd62b` | **P6** kevy-resp-client | malformed-reply integration test; cov 91 → 100% (lines/fns/branches) |
| `2a1d9a8` | **P7** kevy-map  | hashbrown cohort — competitive (tied at n=4k get-hit), behind on insert / small-table / large-table (SIMD group scan deferred to v0.1.1) |
| `910208f` | **P8** kevy-uring | Linux-only; perf data deferred to lx64 metal harness |
| `c031483` | **E** e2e re-bench | mac docker: PING +65-70% (resp parser transmits); -c50 SET/GET regressed (likely Docker noise; lx64 metal re-bench is the publish gate) |

## Stone-by-stone publish readiness

| stone              | perf vs best competitor                | cov              | miri        | BASELINE doc | publish-ready? |
|--------------------|----------------------------------------|------------------|-------------|--------------|----------------|
| kevy-bench         | dev-tool, no gate                      | n/a              | n/a         | n/a          | ✅ |
| kevy-bytes         | wins inline + eq; -7 ns vs Go-pool on heap-alloc (allocator-tier) | 98.74% L | 30/30 | ✅           | ✅ |
| kevy-hash          | top tier (tied with ahash/rustc-hash/wyhash/xxh3/std::hash); +3-13× vs SipHash | 100% L | 10/10 | ✅ | ✅ |
| kevy-madvise       | structural (single madvise wrap)        | 100%            | 4/4         | ✅            | ✅ |
| kevy-map           | mid-table get-hit tied; insert + small/large table 6-8 ns behind hashbrown SIMD (v0.1.1 target) | 98.81% L | 33/33 | ✅ | ⚠️ honest gap |
| kevy-resp          | 9× faster than redis-rs                 | 97.27% L        | 25/25       | ✅            | ✅ |
| kevy-resp-client   | network-dominated (delegates to kevy-resp's 9× win) | 100% L | n/a (TCP not in miri) | ✅ | ✅ |
| kevy-ring          | NEW LEAD — cross-thread 4 ns beats rtrb's 5 ns | 100% L | 7/7         | ✅            | ✅ |
| kevy-uring         | Linux-only; integration tests carried from v0.polish | 0% on mac (cfg empty); deferred lx64 | deferred | ✅ | ⏳ lx64 needed |

## What's still blocking the actual `cargo publish` chain

Per the mailrs deep-polish methodology, v0.1.0 publish goes bottom-up
by dep DAG. **The stones are polish-ready; the publish itself is the
next user-gated step**:

1. **Lx64 metal re-bench** (Phase E follow-up) — confirms the
   PING-transmission + checks the mac docker -c50 SET/GET regression
   is Docker noise, not a real cement-path regression. Required before
   `cargo publish` to avoid shipping a known-regressed v0.1.0.
2. **kevy-uring lx64 integration test pass** (Phase P8 follow-up).
3. **`cargo publish -p <stone>` bottom-up** in this order:
   - `kevy-bench` (no kevy-* deps)
   - `kevy-hash`, `kevy-resp`, `kevy-ring`, `kevy-madvise`, `kevy-uring`
     (no kevy-* lib deps among themselves)
   - `kevy-resp-client` (dep: kevy-resp)
   - `kevy-bytes` (dep: kevy-hash)
   - `kevy-map` (dep: kevy-hash + kevy-madvise)
4. **git tag `v0.1.0-deep-polish`** + push to origin.

All four steps are **outside the autorun session's reach** — they
require the user's metal box (lx64) and / or `cargo publish` credentials.

## v0.1.1 + beyond (cold backlog)

- **kevy-map SIMD group scan** — SSE2 (x86_64) / NEON (aarch64)
  16-byte metadata probe. Closes the 6-8 ns insert gap and the small-
  table get-hit gap vs hashbrown.
- **kevy-bytes allocator gap** (-7 ns vs Go runtime pool) — would
  require either switching to a fast bump allocator (charter-OK as
  optional dev feature) or accepting the system-malloc floor.
- **kevy-uring lx64 full bench** vs tokio-uring / liburing / monoio.
- **Cement refactor wave** (v1.5 backlog): kevy-sys 735 LOC,
  kevy-rt/exec.rs 668, kevy/dispatch.rs 523, kevy-rt/shard.rs 516,
  kevy/cmd.rs 468 — currently above the 500-LOC stone bar but
  exempt because they're cement (no publish, no stone audit).

## Methodology validated

This session's run validates the mailrs stone-deep-polish methodology
end-to-end on kevy:

1. **Split → measure → optimise → re-measure** sequence directly
   surfaced two real perf gaps that no purely-internal bench would
   have caught:
   - kevy-ring cross-thread 52 ns (lacking cached cursors) — fixed
     to 4 ns, **now leads the Rust SPSC ecosystem**.
   - kevy-bytes heap-clone 36 ns — fixed to 19 ns (35% faster) via
     specialised Clone path.
2. **Cross-language cohort** prevented inward-looking false wins
   (would have called "≥ valkey" a victory; the cross-lang gate
   forced "≥ max of Rust/Go/C/C++" which is the real ceiling).
3. **Effective coverage discipline** (no padding; each test asserts
   an invariant) lifted kevy-bytes 70 → 98.74%, kevy-hash 93 → 100%,
   kevy-resp 91 → 97%, kevy-resp-client 91 → 100% — all by tests
   that exercise specific contracts (Hash agreement, Borrow lookup,
   malformed-reply error path, etc.), not by padding.
4. **Cohort-aware gate framing** (e.g., kevy-bytes vs Vec<u8> /
   sds / Go []byte = byte-cohort vs std::String / smol_str =
   shared-cohort) prevented unfair comparisons of fundamentally
   different semantics.

The methodology IS the win. The data is the audit trail.
