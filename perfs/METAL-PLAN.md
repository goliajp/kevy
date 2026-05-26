# kevy → metal: exhaustive perf/mem/size refactor (L1–L4 linear task set)

Authorized 2026-05-26: ignore ROI / win-risk. Try every metal-level lever; let
perf/mem/size data speak. **Correctness stays a hard gate** (sharded 11/11 epoll+
io_uring, full workspace tests, clippy 0 per checkpoint) — only the perf-WIN gate
is relaxed (try uncertain checkpoints; keep what helps any of perf/mem/size, revert
only clear all-axis regressions; document every result in `perfs/`).

## L1 — Roadmap (one line)
Close the gap between kevy's current per-core (~169 ns/cmd ≈ ~590 cyc, ~3–4× above
the compute floor; single-shard 5.9M GET/core) and the **hardware ceilings** —
compute floor, memory latency, cross-core coherency, NIC zero-copy — measured on
lx64, optimizing perf + mem + size exhaustively.

## L2 — Version boundary `v0.metal` (rewritten 2026-05-26 from perf-flat data)

Scope: pure perf/mem/size of the existing feature set. No new commands/features.
Charter intact (0 crates.io dep, pure-Rust, libc only in kevy-sys, no C).

**Re-ordered 2026-05-26 PM** after the v0.metal-4+5 perf record on the 1-key
cache-hot path (see `perfs/data/2026-05-26/perf-flat-after-metal-4-5.txt`).
The flat self-time map exposed a much bigger lever than the remaining memory-
wall residual:

  parse_command + libc malloc/cfree + Argv::with_capacity   ≈ **32% CPU**
  start_command + handle_command + dispatch_into            ≈ **30% CPU**
  KevyMap::find_by_borrow + Store::get + live_entry         ≈  **2% CPU**

The memory wall has already been beaten down by v0.metal-2/3/4+5 (10M-key
column +21.2% cumulative; bucket-probe miss hidden by prefetch). Remaining
DRAM-bound levers (hugepages, SSE2 group scan) only move the 10M column.
The **hot-path CPU levers (parse + dispatch ~ 60% combined) move every
column**, including the 1-key cache-hot one we couldn't touch with mem-wall
levers. So Phase A leads. Each checkpoint targets one lever (NOT a "stone" in
the mailrs ARCHITECTURE.md sense — these checkpoints are cement-internal
perf work, not extractable foundation crates). Correctness gate stays hard,
perf-WIN gate relaxed (keep any-axis win, revert only all-axis regression,
document everything).

### Phase A — Hot-path CPU (ceiling: highest; lifts every keyspace column)

- ✅ **v0.metal-1 — Measurement foundation.** Done — commit 2db0b27.
- ✅ **v0.metal-2 — Box collection Value variants.** Done — commit 2c3ee26.
- ✅ **v0.metal-3 — Value inlining (SmallBytes SSO).** Done — commit fb7c71c.
- ✅ **v0.metal-4+5 — kevy-map + bucket prefetch.** Done — commit 421c826.
- ✅ **v0.metal-6 — Zero-alloc parse + scratch `Argv`.** Done — commit 3659569.
  Argv::with_capacity 1.51% → 0%; 100k +7.7% / 1M +5.1% / 10M +2.7% vs metal-4+5.
- ✅ **v0.metal-7 — SWAR find_crlf.** Done — commit bfa388d. 8-byte SWAR with
  has-zero-byte detection in kevy-resp (SSE2 skipped to keep `forbid(unsafe_code)`).
  Modest +3.8% on 100k; documented insight that further parse gains need
  borrowed argv (avoid extend_from_slice byte copy).
- ✅ **v0.metal-8 — Dispatch redo (resolve + shard_of).** Done — commit 2a9bfae.
  `Commands::resolve` collapses 4× upper_verb to 1×; `shard_of` short-circuits
  on single-shard + uses KevyHash for multi-shard. **start_command 18.70% → 7.11%
  (-11.59pp)**; dispatch chain -30% relative. 1-key +5.2% (recovers most of the
  earlier -7% drift); 1M +7.4%. Cumulative vs metal-1: 1-key -2.4%, 100k +18.3%,
  1M +19.2%, 10M +23.2%.

### Phase B — DRAM-bound residual (ceiling: medium; only 10M-key column)

- ✅ **v0.metal-9 — Hugepages (THP madvise).** Done — commit 3a29cf6.
  10M +2.5% / 1-key first crossed into +3.5% vs baseline; RSS neutral.
- **v0.metal-10 — KevyMap SSE2 group scan.** RFC step 6 carry-over from
  v0.metal-4+5. Now `KevyMap::find_by_borrow` is < 1% of CPU (prefetch
  already hides the bucket-probe miss), so this is a small lever; ship for
  completeness and instruction-cache density. Expected: +2-5%.

### Phase C — Reactor / IO (ceiling: medium)

- ✅ **v0.metal-11 — Reactor loop micro-ops.** Done — commit f71b9ae.
  flush_* outer-empty short-circuits + `events` mem::take guarded on
  non-empty. 10M +4.3% (DRAM-bound path benefits most); other columns
  in paired noise.
- **v0.metal-12 — io_uring zero-copy IO.** `SEND_ZC` for replies;
  registered files (fixed fd, skip table lookup) + registered buffers;
  revisit multishot. lx64 NIC = 100 Mbit (correctness verifiable on
  loopback, throughput not). Big change to `uring_reactor.rs`; in-charter
  (io_uring is the syscall interface).

### Phase D — Mem footprint extremity (ceiling: medium-high on RSS)

- ✅ **v0.metal-13 — Key inline (SmallBytes for K).** Done — commit 1d9d655.
  Headline mem-axis win: RSS -17.3% (-254 MB) at 8.6M keys; also gave
  +9-19% on middle/large keyspace perf columns because hash + key compare
  collapse into one cache line.
- **v0.metal-14 — Per-conn + pbuf-ring tightness.** Default input/output
  buffers per conn × shards = a few MB; io_uring pbuf-ring also generous.
  Shrink defaults, grow on demand, shrink-to-fit on idle. Expected: RSS
  -50-100 MB in many-conn scenarios. A/B for perf trade-off.
- **v0.metal-15 — KevyMap load-factor + bucket layout review.** Currently
  7/8 LF. Analytical pre-check: at 8.6M keys / next_pow2(8.6M/0.875)=16M,
  LF up to 0.9375 keeps cap at 16M — no RSS movement at this keyspace
  size from LF alone. Run only as a perf experiment if budget allows.

### Phase E — Binary size + closing

- **v0.metal-16 — Binary size sweep.** Use `nm` + `c++filt` (or
  `cargo-bloat` if installed locally as a dev tool) to enumerate fn
  bytes; trim `KevyMap<K, V>` monomorphisation explosion; evaluate
  `opt-level = "z"`/`"s"`/`"3"` perf vs size; dead-code prune. Current
  lx64 stripped = 666 KB; expected -20-40%.
- **v0.metal-17 — Cross-core arena.** Lowest ceiling (ring 6-9 ns/item on
  x86); pool alloc on the forward path; shrink bytes per hop. Mem-axis
  side win too. Closes the checkpoint list.

## L3a — HOT plan (current checkpoint: v0.metal-16 — binary size sweep)

After metal-9 (hugepages, done), metal-13 (key inline, done), metal-11
(reactor micro-ops, done), remaining open checkpoints are:

  metal-10 SSE2 group scan      KevyMap::find_by_borrow < 1% → ceiling tiny
  metal-12 io_uring zero-copy   lx64 NIC 100Mbit ⇒ throughput unmeasurable
  metal-14 per-conn buffers     mem -50-100MB additional; RSS already -46% from baseline
  metal-15 LF revisit           analytically zero RSS movement at current keyspace
  metal-16 binary size          UNTOUCHED axis ("perf/mem/size 三轴极致")
  metal-17 cross-core arena     1-2% perf, lowest

`metal-16` is the only checkpoint that addresses the size axis at all
(11 done, 0 size touches) and is contained (Cargo.toml + dead-code
sweep). Promote.

Hot plan (6 steps):

1. **Branch.** `git flow feature start metal-16-size`.
2. **Establish baseline.** On lx64, build release stripped + with
   debug syms; record `ls -l` byte count + `size -A` per-section
   breakdown into `perfs/data/2026-05-26/metal-16-baseline.txt`.
3. **Bytes-by-symbol audit.** Use `nm --print-size --size-sort
   --reverse-sort --demangle target/release/kevy | head -40` to
   identify the top 40 largest symbols. Look for: monomorphisation
   explosions (multiple instantiations of KevyMap), large unused
   functions, large generic vtables.
4. **opt-level trade-off.** Try `opt-level = "s"` and `"z"` in a
   `[profile.release-size]` profile (don't replace `release`). Build
   with each, measure binary size + run 3-run A/B at one keyspace
   (1-key / 10M). Choose the best trade-off (size win + perf <5%
   regression) for the shipped profile.
5. **Dead code sweep.** Apply findings from step 3: gate optional
   features behind `cfg`, remove unused trait impls, see if any large
   fn can be `#[cold]` to push it out of icache hot path.
6. **lx64 A/B (3-run medians).** rps unchanged (or ≤5% paired
   regression); record final stripped binary size. Data file
   `metal-16-binary-size-ab.txt`. Judge + finish.

## L3a (previous, completed) — v0.metal-9 / -11 / -13 brief recap

- metal-9 hugepages: kevy-sys advise_hugepage + kevy-map alloc_table call.
- metal-11 reactor: flush_* outer-empty short-circuits + events mem::take guard.
- metal-13 key inline: KevyMap<Vec<u8>, V> → KevyMap<SmallBytes, V> across
  Store::map / HashData / SetData / ZSetData.by_member.

(was: HOT plan for v0.metal-9 hugepages — now archived as done.)

## L3a (older, completed) — v0.metal-9 hugepages original hot plan

After v0.metal-8, the post-flat reads:

  parse_command_into  14.27%
  find_crlf            8.59%   (SWAR'd, standalone now visible)
  reactor::run        10.85%
  dispatch chain      20.57%   (start 7.11 + handle 5.19 + dispatch_into 4.37 + resolve 3.90)

Phase A's wins are concentrated on the dispatch chain (-30% relative on
the chain). `parse_command_into` still has the `extend_from_slice`
byte-copy — further parse gains need a true borrowed argv (out of
scope as a single checkpoint; tracked as a Phase D refactor candidate).

Phase B's hugepages checkpoint is the simplest next move: environment-level
change, no code risk, hits 10M-key column directly via TLB. Promote it
next.

1. **Branch.** `git flow feature start metal-9-hugepages`. **Detect**:
   `git branch --show-current` = `feature/metal-9-hugepages`.
2. **THP madvise hook.** In `kevy-store`/`kevy-map` allocators (or
   wherever the keyspace backing-store memory is materialised), call
   `madvise(MADV_HUGEPAGE)` on large allocations. kevy-sys gets a thin
   wrapper around `madvise(2)` (charter-allowed; libc-only OS boundary).
   **Detect**: `cargo test -p kevy-sys` covers the madvise wrapper.
3. **Allocate-and-mark sites.** kevy-map's metadata + slots Box<[T]>
   allocations are the headline targets; per-conn buffers and pbuf-ring
   are lesser candidates (in scope for metal-14, leave alone here).
   Apply `madvise(MADV_HUGEPAGE)` on KevyMap::alloc_table when the
   table crosses ≥ 1MB (huge-page-sized). **Detect**: workspace tests
   pass; sharded 11/11 epoll + io_uring.
4. **Explicit huge pages (optional, only if THP path is weak)**: pull
   from `/proc/sys/vm/nr_hugepages`. SKIP unless THP shows < +3%.
5. **Local correctness gate.** **Detect**: `cargo test --workspace`
   100% pass; clippy 0.
6. **lx64 gate.** Rsync + release w/ debug syms; sharded 11/11 both
   reactors. **Detect**: 22/22.
7. **lx64 A/B (3-run medians ABA).** Re-run metal-8's binary alongside
   to get paired noise band. THP should mainly move the 10M-key
   column (the TLB-pressured one). **Detect**:
   `perfs/data/2026-05-26/metal-9-hugepages-ab.txt` written; perf flat
   shows TLB-miss count drop (use `perf stat -e dTLB-load-misses`).
8. **Judge + merge.** Any axis improved (no axis ≥5% paired
   regression) → `git flow feature finish`. All-axis regression →
   discard + rejection note.

## L3a (previous, completed) — v0.metal-8 dispatch redo

Each step ends with a detection command. Output →
`perfs/data/2026-05-26/metal-8-*`.

1. Branch.
2. `shard_of`: single-shard short-circuit + KevyHash for multi-shard.
3. `Commands::resolve` trait method + KevyCommands override.
4. `handle_command` → call resolve once; pass `ResolvedCmd` to
   `start_command`; replace `self.commands.is_*` calls with field reads.
5. Local + lx64 correctness gates.
6. 3-run A/B + perf flat (start_command 18.70% → 7.11%).
7. Judge KEEP + finish — commit 2a9bfae.

## L3a (older, completed) — v0.metal-7 SWAR find_crlf

Each step ends with a detection command. Output →
`perfs/data/2026-05-26/metal-7-*`.

The remaining parse cost after metal-6 is the byte-by-byte work inside
`parse_multibulk_into`: `find_crlf` (scalar loop) gets called once per
length-line + once per bulk-string-terminator, and `parse_int` (ASCII →
i64) on every length-line. SWAR/SIMD on these scans + a tighter int
parser is the lever.

1. **Branch.** `git flow feature start metal-7-parse-swar`. **Detect**:
   `git branch --show-current` = `feature/metal-7-parse-swar`.
2. **SWAR `find_crlf`.** Rewrite to scan 8 bytes at a time using the
   "byte-equality bit-trick" (XOR with 0x0D0D…0D, then the standard
   has-zero-byte detection). Keep the scalar tail for the last < 8
   bytes. Stable Rust only (no `core::arch`). **Detect**: existing
   `kevy-resp` parser tests still pass; add fuzz-like unit tests
   (random inputs with planted CRLFs at known offsets).
3. **SSE2 fast path** for `find_crlf` under `#[cfg(target_arch =
   "x86_64")]`. 16-byte `_mm_loadu_si128` + `_mm_cmpeq_epi8` against
   '\r', then `_mm_movemask_epi8` + `trailing_zeros` for first hit.
   Falls back to SWAR on non-x86. **Detect**: same tests pass; the
   SWAR vs SSE2 path can be selected via cfg-test in CI.
4. **Tight `parse_int`.** Current is a byte-by-byte ASCII loop. Replace
   with a SWAR digit-pack trick for ≤ 8-digit ints (covers every RESP
   length-line we'll see) and falls back to scalar for longer. **Detect**:
   `kevy-resp` parser tests + a fuzz unit testing every i64 value
   round-tripping through write_int + parse_int.
5. **Local correctness gate.** **Detect**: `cargo test --workspace`
   100% pass; clippy 0.
6. **lx64 gate.** Rsync + release build w/ debug syms; sharded 11/11
   epoll + io_uring. **Detect**: 22/22.
7. **lx64 A/B (3-run medians).** **Critical**: also re-measure metal-6
   itself (3 fresh runs) so the comparison is ABA-paired and the 1-key
   noise question gets a real answer. **Detect**:
   `perfs/data/2026-05-26/metal-7-parse-swar-ab.txt` written with paired
   medians + new perf flat.
8. **Judge + merge.** Any axis improved (and no axis ≥ 5% median
   regression in paired ABA) → `git flow feature finish`. All-axis
   regression → discard + rejection note.

## L3a (previous, completed) — v0.metal-6 zero-alloc parse

Each step ends with a detection command. Output →
`perfs/data/2026-05-26/metal-6-*`.

1. Branch. `feature/metal-6-zero-alloc-parse` (now merged).
2. `Argv::clear()` + `reserve_for()` + `parse_command_into(&[u8], &mut Argv)`
   in kevy-resp.
3. `Shard.scratch_argv: Argv` field.
4. `conn_readable` / `uring_on_recv`: parse via `parse_command_into` +
   `mem::take` for dispatch + restore.
5. `handle_command` / `start_command`: `args: Argv` → `args: &Argv`;
   cross-shard forwards clone at the boundary.
6. cargo test --workspace + clippy 0 (local).
7. lx64 sharded 11/11 epoll + io_uring.
8. lx64 A/B 3-run medians + perf flat (Argv::with_capacity 1.51% → 0%).
9. Judge KEEP + finish — commit 3659569.

## L3a (previous, completed) — v0.metal-3 value inlining SSO

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

## L3b — COLD plan (v0.metal-7 … v0.metal-17)

As listed in L2 — what / requirement / resource, **not** step-level (expanded
to L3a on promotion). Notes:

- **Phase A residual (metal-7, metal-8)**: lx64-measurable, internal. After
  metal-6 ships, re-take a perf flat to recheck the lever percentages — parse
  and dispatch may shift after the borrowed-argv change.
- **Phase B (metal-9 hugepages, metal-10 SSE2)**: hugepages is environment +
  one madvise; SSE2 group scan is contained in `kevy-map` (RFC step 6
  carry-over). Both only meaningful on the 10M-key column.
- **Phase C (metal-11 reactor, metal-12 io_uring zero-copy)**: metal-11 is
  small tuning; metal-12 is a large change in `uring_reactor.rs`. lx64 NIC
  100 Mbit means zero-copy verifies correctness only; throughput needs a
  faster NIC (out-of-scope right now).
- **Phase D (metal-13 key SmallBytes, metal-14 buffers, metal-15 LF review)**:
  metal-13 reuses kevy-bytes; the others are tuning. metal-15 is a small,
  data-driven adjustment.
- **Phase E (metal-16 size, metal-17 cross-core arena)**: closing checkpoints.
  metal-16 is `cargo-bloat` analysis + opt-level tuning. metal-17 is the
  lowest-ceiling checkpoint left.

## L4 — Triggers (cold → hot promotion predicates)

- 2→3 / 3→4+5 / 4+5→6: **satisfied** (each previous checkpoint merged with a
  data file + sharded 11/11 both reactors + clippy 0).
- N→N+1 (general form): previous merged to develop, A/B data file under
  `perfs/data/2026-05-26/metal-<N>-*.txt` with median rps + RSS + binary
  size; correctness gates green. On promotion, expand the L3b entry into
  a linear L3a hot plan, then proceed.
- **Re-measure trigger**: after each Phase-A checkpoint (metal-6, -7, -8),
  re-run perf flat on the 1-key cache-hot path. If the levers re-rank
  (parse drops out, dispatch surfaces, etc.), update L2's Phase-A order
  before promoting the next checkpoint. Once Phase A's combined `parse +
  dispatch` CPU share falls below ~15%, Phase A is done — proceed to
  Phase B (DRAM residual).

Autorun: execute L3a; at each checkpoint completion check the L4 predicate,
then promote the next. **Per-session authorization (2026-05-26)**: when a
step presents a fork, pick the **higher-ceiling** option without pausing.
Stop and report only if (a) a measurement falsifies the plan, (b) all-axis
regression triggers revert, or (c) charter would be violated.

## Expected cumulative end-state (vs current develop after metal-4+5)

| Axis              | Now (lx64 1-shard io_uring) | Phase A done | Phase B done | Phase C-E done |
|-------------------|-----------------------------|--------------|--------------|----------------|
| 1-key GET rps     | 4.6M                        | **6.5-7.5M** | +marginal    | +marginal      |
| 10M-key GET rps   | 2.77M                       | 3.8-4.5M     | **4.1-5.0M** | +marginal      |
| RSS @ 8.6M keys   | 1.47 GB                     | ~1.45 GB     | ~1.42 GB     | **1.20-1.30 GB** |
| Stripped binary   | 655 KB                      | 670 KB       | 680 KB       | **~400 KB**    |

Numbers are pre-measurement estimates; the data files under
`perfs/data/2026-05-26/` are the ground truth.
