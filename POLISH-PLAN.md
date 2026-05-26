# kevy → polish: stones + cement audit + closure (v0.polish) — ✅ CLOSED 2026-05-26

> **Status**: v0.polish complete. All Phase A/B/C/D gates green; see
> `RELEASE-DRY-RUN.md`, per-stone `STONE-STATUS.md`, and per-crate
> `AUDIT-2026-05-26.md` for evidence. Carry-over backlog tracked in
> `ROADMAP.md`.


## L1 — Roadmap (one line)

Take every kevy crate to the bar its category demands (STONE-AUDIT for
stones, CEMENT-AUDIT for cement, light review for dev tools), so the
project is **inspectable + publishable** when we decide to publish.

## L2 — Version boundary `v0.polish` (locked scope)

Scope:
- 4 stones (`kevy-hash`, `kevy-ring`, `kevy-resp`, `kevy-resp-client`)
  pass STONE-AUDIT T1+T2. The other 2 (`kevy-bytes`, `kevy-map`) already
  passed in this session.
- 5 cements (`kevy-sys`, `kevy-store`, `kevy-persist`, `kevy-rt`, `kevy`)
  pass CEMENT-AUDIT T1+T2.
- T3 PUB on all 6 stones (READMEs / CHANGELOGs / PLATFORMS / MEM-BUDGETS
  / STONE-STATUS).
- Pipeline rehearsal: pick one stone, do `cargo publish --dry-run` +
  smoke install in a clean env. **Don't actually publish.**

Out of scope (do NOT scope-creep into):
- New perf checkpoints (v0.metal already exhausted within current testbed).
- New Redis features (XADD streams / CLUSTER / scripting / AUTH).
- Second testbed / NIC upgrade.
- Borrowed-Argv deep refactor (cement perf round-2).
- Actually publishing to crates.io (deferred to v0.publish).

## L3a — HOT plan (Phase A: stone audits, fully linear)

Order: smallest + simplest first to build momentum. Each step opens its
own feature branch, runs the 8-dim STONE-AUDIT, writes
`crates/<name>/AUDIT-2026-05-26.md`, closes T1+T2 gaps in-band,
commits + finishes.

1. **kevy-hash audit + close** (276 LOC, leaf, already has a fmix64
   avalanche guard test). Expected: most green; missing-docs maybe.
   **Detect**: `cargo test -p kevy-hash` + `cargo clippy -Dwarnings` +
   `cargo doc -p kevy-hash` 0 + cov ≥ 90% + miri 100% pass + bench file +
   perf_gate test + BUDGETS.md.
2. **kevy-ring audit + close** (366 LOC, SPSC ring, unsafe).
   Expected: missing-docs + bench gap + maybe needs loom test for
   cross-thread (the actual production race surface).
   **Detect**: STONE-AUDIT T1+T2 + `loom test` (added) cross-thread
   producer/consumer.
3. **kevy-resp-client audit + close** (~75 LOC, freshly carved).
   Expected: bench gap (round-trip ns vs raw `valkey-cli` syscall +
   parse), maybe a from-fixture test.
   **Detect**: STONE-AUDIT T1+T2 + bench against a localhost echo or
   recorded fixture.
4. **kevy-resp audit + close** (687 LOC, RESP2 codec — biggest stone).
   Expected: T1+T2 + **fuzz target** (per STONE-AUDIT §3 parser rule),
   running ≥ 1h on `parse_command` + `parse_reply`.
   **Detect**: STONE-AUDIT T1+T2 + `cargo fuzz run parse_command -- -max_total_time=3600`
   no panics/OOMs.

## L3b — COLD plan (Phase B: cement audits; Phase C: T3 PUB; Phase D: publish rehearsal)

### Phase B (cement audits) — 5 cements

After Phase A. Run CEMENT-AUDIT against each cement, write
`crates/<name>/AUDIT-2026-05-26.md`, close T1+T2 gaps.

1. **kevy-sys** — special: extra FFI-vs-manpage review block; no miri.
2. **kevy-persist** — Cement standard.
3. **kevy-store** — Cement standard + BUDGETS.md (hot-path).
4. **kevy-rt** — Cement standard + BUDGETS.md (hot-path).
5. **kevy** — binary, lightest cement audit (mostly serves cargo
   compose / serve glue).

### Phase C (stone T3 PUB) — 6 stones

After Phase A. For each stone:
- `README.md`: one-page (identity / install / minimal example / "why-not-X" comparison)
- `CHANGELOG.md`: 0.1.0 entry
- `PLATFORMS.md`: supported targets + caveats
- `MEM-BUDGETS.md`: per-op heap numbers (if mem axis is meaningful for that stone)
- `STONE-STATUS.md`: miri / fuzz timestamp + verdict
- `Cargo.toml`: set `readme = "README.md"`

### Phase D (publish-pipeline rehearsal) — pick 1 stone, dry-run

After Phase B+C. Pick **kevy-bytes** (cleanest identity, smallest LOC,
already T1+T2+T3 ready):

1. `cargo publish --dry-run -p kevy-bytes` — verify the tarball
2. In a scratch dir: `cargo new smoke && cd smoke && cargo add kevy-bytes --path ../crates/kevy-bytes` and call `SmallBytes::from_slice` from a `main.rs`. (Real `cargo add` from crates.io waits for actual publish; this is the path-based smoke.)
3. Document rehearsal output in `RELEASE-DRY-RUN.md` for the future
   actual-publish session.

## L4 — Trigger predicates

- **A → B**: all 4 stone audit files committed + each closes with
  GATE/BAR ✅ in the verdict.
- **A or B → C**: each individual stone reaches T2 ✅ → its T3 PUB
  can start in parallel (they don't block each other).
- **C → D**: kevy-bytes T3 PUB done (READMEs+CHANGELOG+ etc.) + the rest
  of the stones at any T2-clean state.
- **D → close v0.polish**: rehearsal output recorded; no real publish.

## Order recap (full linear path through v0.polish)

```
[done] kevy-bytes audit + T1 + T2
[done] kevy-map audit + T1 + T2
1. kevy-hash audit + T1 + T2          ← HOT, next step
2. kevy-ring audit + T1 + T2 + loom
3. kevy-resp-client audit + T1 + T2 + roundtrip bench
4. kevy-resp audit + T1 + T2 + fuzz (1h)
5. kevy-sys cement audit + FFI review
6. kevy-persist cement audit
7. kevy-store cement audit + BUDGETS.md
8. kevy-rt cement audit + BUDGETS.md
9. kevy cement audit (light)
10. Stone T3 PUB ×6 (parallelizable per stone once T2 green)
11. kevy-bytes publish dry-run + smoke install rehearsal
[end of v0.polish]
```

## Estimation

- Phase A: 4 stones × ~1-2 commits/stone = 8-12 commits + bench/test/fuzz
  cycles for resp (the fuzz step is the longest single wait — 1h fuzz
  is mandatory).
- Phase B: 5 cements × ~1 commit/cement (mostly already meets cement
  bar from existing testing) = 5 commits.
- Phase C: 6 stones × 5 files ≈ 30 files; can bulk if templates are
  shared = ~6-12 commits.
- Phase D: 1 commit + a rehearsal markdown.

Total: ~25-40 commits to close v0.polish. **End state: every crate
either publishable (stone) or maintainable (cement), and we know
publishing works.**
