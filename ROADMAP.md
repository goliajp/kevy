# kevy roadmap

Forward-looking work after **v0.polish** (closed 2026-05-26). This file is
the L3b cold-backlog log: what's queued, why, and what would unblock it.
Hot work-in-progress is tracked in `POLISH-PLAN.md`-style boundary
documents, not here.

## v0.publish — actually publish to crates.io

Trigger: token + name claim + version bump.

Order is documented in [`RELEASE-DRY-RUN.md`](./RELEASE-DRY-RUN.md):

```
kevy-bench → kevy-hash → kevy-resp → kevy-ring → kevy-sys
           → kevy-bytes → kevy-resp-client → kevy-map → kevy
```

Sub-items:
- Decide whether `kevy-sys` graduates from cement to a publishable stone.
  Currently it carries the project-specific identity "kevy's OS-boundary
  libc wrap"; if generalized to "minimal pure-Rust libc bindings without
  the `libc` crate", it could be its own stone. Until then, it ships
  alongside `kevy` the binary, not as a standalone publish.
- Re-run `cargo package -p <stone>` end-to-end (verification compile)
  after each layer publishes — the rehearsal in RELEASE-DRY-RUN.md
  documented only `kevy-bench` because the layers above need their deps
  on the actual registry.
- Tag `v0.1.0` after the chain completes.

## Coverage carry-overs from v0.polish

These are cement-bar-passing (≥80% line cov per file) but worth tightening:

- **`crates/kevy-rt/src/shard.rs`** — 76.22% line cov. Integration tests
  (sharded.rs) exercise the reactor; the gap is the **mock-poller code path**
  + a couple of degenerate-cap branches. Closing would need a unit-level
  mock for `kevy_sys::Poller` (one-shot stub). Cement bar already passes
  in aggregate.
- **`crates/kevy/src/lib.rs`** — 70.24% line cov. The `Commands` impl +
  `serve` glue. Sharded + compat3 cover it end-to-end; missing in
  llvm-cov because compat3 is shell-driven. Closing would need
  in-process `KevyCommands::route` / `resolve` matrix tests.
- **`crates/kevy-store/src/list.rs`** — 87.06%. Some splice / iter edge
  cases. Cement bar already passes.

## Performance backlog

- **lx64 x86_64 numbers in BUDGETS.md** — most BUDGETS files only carry
  mac aarch64 readings. The lx64 production numbers exist in
  `perfs/data/2026-05-26/metal-*` but aren't aggregated into per-crate
  BUDGETS yet. Add a "lx64 x86_64" column to each.
- **`kevy-store` + `kevy-rt` BUDGETS.md** — neither has its own yet
  (per-crate). The system-level numbers in `bench/REPORT.md` cover them
  jointly; per-crate aggregation would name which crate contributes which
  shard of a flat profile.
- **Borrowed-Argv refactor (cement perf round-2)** — deferred from
  v0.polish. The current `Argv` allocates `buf` + `ends` on first use;
  borrowing the parser's read buffer would drop the `buf` alloc in the
  warmed hot path. Estimated +1–3% on `-c50 SET/GET`; needs lifetime
  threading through `Store` API.
- **Second testbed / NIC upgrade** — deferred. lx64 is single-host; many
  perf claims assume the NIC isn't the bottleneck. A 25/40 Gbe rig +
  cross-host bench would unblock the next perf checkpoint.
- **io_uring backend** for `kevy-sys::Poller` — north-star perf goal is
  hardware disk-I/O ceiling (see memory `project-kevy-perf-northstar`).
  Requires Linux 5.6+; would replace the current `epoll` shim.

## Hardening backlog

- **alloc-count test for `SmallBytes`** — swap a counting `GlobalAlloc`,
  assert inline path has 0 allocs on a tight loop. Noted in
  `crates/kevy-bytes/AUDIT-2026-05-26.md`.
- **Loom enumeration tests** — currently deferred (charter conflict:
  `loom` is a crates.io dep). If a "dev-only with charter exception"
  policy is articulated (the way `cargo-fuzz` is), `kevy-ring` + the
  per-shard inbox in `kevy-rt` are the candidates.
- **Project-wide CI** — no GitHub Actions / GitLab CI workflow yet. The
  per-stone AUDIT files note that the matrix
  (`x86_64-linux + aarch64-linux + macos`) is not asserted by CI.
- **Cross-arch fuzz** — fuzz is currently mac aarch64 only; the lx64
  production target has a different optimizer profile. Adding a Linux
  fuzz job (probably via CI) would re-fuzz the codec under that profile.

## Functional backlog (v2 boundary)

Per memory `project-kevy-v2-plan`:

- **5 data types**: string ✅ / hash ✅ / list ✅ / set ✅ / zset ✅ — all
  shipped in v1.
- **Pub/sub** ✅ shipped.
- **Transactions (MULTI / EXEC / DISCARD)** — needs cement audit
  follow-up to confirm cross-shard semantics.
- **Cross-shard multi-key (MSET / MGET / SINTER / SUNION / SDIFF / KEYS /
  SCAN / RANDOMKEY)** ✅ shipped.
- **AUTH / TLS** — out of scope for v0.polish; deferred to a future
  "compat + security" round.
- **Cluster mode** — not on the v2 boundary. Adding it changes the
  single-trust-domain assumption that `kevy-hash`'s no-DoS choice is
  predicated on; would need re-thinking the hasher.

## Documentation backlog

- **Top-level README** — there's no project-root `README.md` yet (only
  per-stone READMEs). It should explain the layered architecture (stones
  → cements → binary), the perf headline, and how to build/run.
- **MIGRATION-FROM-VALKEY.md** — `bench/compat3.sh` already validates
  94-cmd parity reply-by-reply against valkey 9.1; a one-page mapping
  doc would unblock external adopters.
- **`docs.rs` per stone** — once published, the per-stone `documentation`
  workspace field points at `https://docs.rs/<stone>` automatically. Add
  a project-level index pointing at all 6.
