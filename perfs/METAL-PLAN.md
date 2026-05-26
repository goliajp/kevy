# kevy → metal: exhaustive perf/mem/size refactor (L1–L4 linear task set)

Authorized 2026-05-26: ignore ROI / win-risk. Try every metal-level lever; let
perf/mem/size data speak. **Correctness stays a hard gate** (sharded 11/11 epoll+
io_uring, full workspace tests, clippy 0 per checkpoint) — only the perf-WIN gate
is relaxed (try uncertain stones; keep what helps any of perf/mem/size, revert
only clear all-axis regressions; document every result in `perfs/`).

## L1 — Roadmap (one line)
Close the gap between kevy's current per-core (~169 ns/cmd ≈ ~590 cyc, ~3–4× above
the compute floor; single-shard 5.9M GET/core) and the **hardware ceilings** —
compute floor, memory latency, cross-core coherency, NIC zero-copy — measured on
lx64, optimizing perf + mem + size exhaustively.

## L2 — Version boundary `v0.metal` (data-informed reorder after v0.metal-1)

Scope: pure perf/mem/size of the existing feature set. No new commands/features.
Charter intact (0 crates.io dep, pure-Rust, libc only in kevy-sys, no C). The
checkpoints below are **reordered against the original linear plan** based on
`perfs/data/2026-05-26/metal-baseline.txt` — the memory wall is the dominant
lever (-52%), cross-core ring is cheap on x86 (6-9 ns/item; deprioritized). Each
checkpoint is its own stone; perf-WIN gate relaxed but correctness gate stays.

- ✅ **v0.metal-1 — Measurement foundation.** Large-keyspace bench, ring micro-
  bench, perf-on-lx64 harness, RSS + binary-size tracking. Done — commit 2db0b27.
- ✅ **v0.metal-2 — Box collection Value variants.** Hash/List/Set/ZSet boxed →
  Entry 80→48B → RSS -29% @ 8.6M keys, 10M-key GET +2.4%. Done — commit 2c3ee26.
- **v0.metal-3 — Value inlining (SSO).** A 24B small-byte-string type (inline
  ≤22B, else heap) lives in a new `kevy-bytes` crate (unsafe union; kevy-store
  stays `forbid(unsafe_code)`). Replaces `Value::Str(Vec<u8>)`. Kills the small-
  string-value pointer-chase 2nd cache miss on GET. Must keep `size_of::<Value>()`
  ≤ 32B (don't undo box-collection's Entry-48B win).
- **v0.metal-4 — Self-built `kevy-map` hashtable.** Pure-Rust open-addressing
  Swiss-style table replacing std `HashMap`. Per-shard, single-thread, no
  DoS-hardening tax. **Unlock**: exposes bucket addresses, the precondition for
  software prefetch (v0.metal-5). Also removes any std SipHash residue + lets us
  control bucket layout.
- **v0.metal-5 — Software prefetch + cache-conscious bucket layout.** In batch
  processing, `prefetch(next_key_bucket)` while finishing the current. Co-design
  bucket layout (key inlined? metadata grouped? cache-line packing?) using
  `kevy-map`'s freedom. The direct hammer on the -52% memory wall.
- **v0.metal-6 — Hugepages (THP / explicit).** Large pages for the store backing
  to drop TLB miss rate at 10M+ keys. Environment tuning; independent of code.
- **v0.metal-7 — Zero-alloc local hot path.** Kill parse's 2 per-command allocs
  on the LOCAL path (borrow argv from the input buffer; owned only when
  forwarded). Profile shows malloc/cfree ~10%.
- **v0.metal-8 — Parse to the floor.** SWAR CRLF + length scan; single-pass.
  Profile shows parse_command ~13.5%.
- **v0.metal-9 — Dispatch to the floor.** Branch-lean / perfect-hash verb
  dispatch replacing the `dispatch_* || …` chain.
- **v0.metal-10 — Cross-core arena / msg compaction.** Pool alloc on the forward
  path; shrink bytes + cache lines per hop. Low priority (ring 6-9 ns/item on
  x86) but still try.
- **v0.metal-11 — Zero-copy IO.** io_uring `SEND_ZC` for replies; registered
  files + registered buffers; revisit multishot. lx64 NIC = 100Mbit (correctness
  verifiable, throughput not). AF_XDP = stretch.
- **v0.metal-12 — Footprint & size.** Per-conn + pbuf-ring + store overhead;
  binary size (LTO / codegen / opt-level / panic strategy) — the size axis.

## L3a — HOT plan (current checkpoint: v0.metal-3 — value inlining SSO)

Each step ends with a detection command. Output → `perfs/data/2026-05-26/metal-3-*`.

1. **Create `crates/kevy-bytes`** — new workspace crate; `Cargo.toml` (rust-version,
   author inherited, no deps); `src/lib.rs` shell + module split (`SmallBytes`).
   **Detect**: `cargo check -p kevy-bytes` clean.
2. **Implement `SmallBytes` (24B unsafe union).** Inline rep: `[u8; 23] + u8 tag`
   (tag = 0..=22 → inline length). Heap rep: `NonNull<u8>` ptr + `usize` len +
   `usize` cap_and_tag (high byte = 0xFF marker, low 56 bits = capacity). Little-
   endian only (compile_error! on BE). API: `new(&[u8])`, `from_vec(Vec<u8>)`,
   `as_slice()`, `to_vec()`, `len()`, `is_empty()`, plus `Default`, `Clone`, `Drop`,
   `PartialEq`/`Eq`, `Hash`, `Debug`. **Detect**: `const _: () = { assert!(size_of::
   <SmallBytes>() == 24 && align_of::<SmallBytes>() == 8); };` compiles; unit
   tests cover inline boundary (0/22/23/lots-of-bytes), drop both reps, clone
   both reps, eq across reps, roundtrip to_vec/from_vec; miri (if available)
   clean.
3. **kevy-store depends on kevy-bytes; replace `Value::Str(Vec<u8>)`.** Edit
   `value.rs` and **add `const _: () = { assert!(size_of::<Value>() <= 32); };`**
   to lock the layout. **Detect**: `cargo check -p kevy-store` clean.
4. **Adapt downstream** in `crates/kevy-store/src/string.rs`,
   `crates/kevy-store/src/lib.rs` (`load_str`), `crates/kevy-persist/src/lib.rs`
   (`write_entry` Value::Str arm). Pattern for mut grow (`append`, `incr_by_float`):
   `let mut v = std::mem::take(slot).into_vec(); v.extend_from_slice(data); *slot
   = SmallBytes::from_vec(v);`. **Detect**: `cargo check --workspace` clean.
5. **Local correctness gate.** **Detect**: `cargo test --workspace` 100% pass;
   `cargo clippy --workspace --all-targets -- -D warnings` 0 findings.
6. **lx64 correctness gate.** Push branch to lx64 (existing `kevy_dev` flow),
   build release w/ debug syms, run sharded suite on both reactors. **Detect**:
   sharded 11/11 on epoll AND io_uring.
7. **lx64 A/B**. (a) Cache-hot GET `-c50 -P256` per-core. (b) `bench/metal_keyspace.
   sh` at N ∈ {1, 100k, 1M, 10M}: rps + RSS via `/proc/<pid>/status` VmRSS.
   (c) Stripped binary size (`ls -l target/release/kevy`). **Detect**:
   `perfs/data/2026-05-26/metal-3-value-inlining-ab.txt` written with before/
   after for all three axes.
8. **Judge + merge.** Any axis improved (and no axis ≥ 5% regressed) → `git flow
   feature finish` to develop, commit msg lists the win(s). All-axis regression
   → `git flow feature` discard + write rejection note in data file. **Detect**:
   develop's `git log -1` shows the metal-3 commit or the data file shows the
   rejection rationale.

## L3b — COLD plan (v0.metal-4 … v0.metal-12)

As listed in L2 — what / requirement / resource, **not** step-level (expanded
to L3a on promotion). Notes:

- v0.metal-4 (kevy-map): biggest single piece of engineering left. Open-addressing
  Swiss layout; control of metadata-bytes/group/probe sequence. Pre-bench against
  std HashMap on the kevy keyspace to size effort.
- v0.metal-5 (prefetch): depends on v0.metal-4 (need bucket address); landing
  zone is `kevy-store::Store::execute_batch` or similar.
- v0.metal-6 (hugepages): runtime + sysctl setup; pure environment.
- v0.metal-7..9 (alloc/parse/dispatch): all internal, lx64-measurable.
- v0.metal-10..12 (cross-core / IO / size): cross-core ring is x86-cheap; IO
  zero-copy bounded by lx64 100Mbit NIC (correctness only); size axis last.

## L4 — Triggers (cold → hot promotion predicates)

- 2→3: **satisfied** (v0.metal-2 merged, baseline data informs the reorder).
- 3→4: v0.metal-3 merged to develop with sharded 11/11 (epoll+io_uring) + clippy
  0 + `perfs/data/2026-05-26/metal-3-value-inlining-ab.txt` exists.
- 4→5: v0.metal-4 (kevy-map) merged with sharded 11/11 + clippy 0 + an A/B file.
  Because v0.metal-5 needs bucket-address API from v0.metal-4, this trigger is
  hard.
- 5→6 … 11→12: same shape — previous merged + correctness + A/B file. On
  promotion expand the L3b entry into a linear L3a hot plan.

Autorun: execute L3a; at each checkpoint completion check the L4 predicate, then
promote the next. **Per session authorization (2026-05-26)**: when a step
presents a fork, pick the option with the **higher ceiling** without pausing —
don't stop to ask. Still stop and report if (a) a measurement falsifies the
plan, (b) all-axis regression triggers revert, or (c) charter would be violated.
