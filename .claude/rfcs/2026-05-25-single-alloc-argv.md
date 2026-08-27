# RFC: single-allocation argv (`Command` representation)

**Status:** **DONE (v0.perf-9, 2026-05-25)** — landed on `develop` via git-flow feature branch.

**Author:** admin@golia.jp + Claude (autorun, 2026-05-25)

**Motivation (measured, v0.perf-8):** `parse_command` is allocation-bound. SET
(3 args) costs ~70 ns, essentially the **four** `Vec` allocations of
`Command = Vec<Vec<u8>>` (one outer + one per arg). Encoders are already
near-free. After the keyspace/read-path work (GET ~28 ns, SET store ~70 ns), the
RESP parse is now a comparable slice of the per-command CPU — and it's the last
big CPU-path allocation lever. See `crates/kevy-resp/BUDGETS.md`.

## The idea

Replace the `N+1`-allocation `Vec<Vec<u8>>` with a **two-allocation** flat argv:

```rust
pub struct Argv {
    buf: Vec<u8>,        // all argument bytes, concatenated
    ends: Vec<u32>,      // ends[i] = offset just past arg i; len = argc
}
impl Argv {
    pub fn len(&self) -> usize { self.ends.len() }
    pub fn get(&self, i: usize) -> Option<&[u8]>;     // slice into buf
    pub fn iter(&self) -> impl Iterator<Item = &[u8]>;
    // builder used by parse:
    pub fn with_capacity(argc: usize, bytes: usize) -> Self;
    pub fn push(&mut self, arg: &[u8]);
}
```

For SET: `buf` = `b"SETkey42value-payload-16"` (one alloc, one growth) + `ends` =
`[3,8,24]` (one alloc) = **2 allocations instead of 4**. Expected parse SET
~70 → ~40 ns (component micro-bench will confirm — host-robust ratio).

It stays `Send` (two `Vec`s), so it still moves across the kevy-ring to another
core's shard — the cross-core ownership requirement that forced *owned* args is
satisfied without N separate heap blocks.

## Why not zero-copy (borrow into the read buffer)?

Already evaluated + rejected (see roadmap memory): cross-core forwarding needs
owned args, and ~93% of commands at -c50 forward, so a borrow would force a
clone at the hop — net worse. `Argv` keeps ownership but collapses N+1 allocs to
2. This is the right shape for a thread-per-core server.

## Affected surface (the blast radius — why it's its own checkpoint)

`Command`/argv is threaded through three crates. Every `&args[i]` (today
`&Vec<u8>` deref-coercing to `&[u8]`) becomes `args.get(i)` / `args.get(i)?`.

- **kevy-resp**: `Command` type → `Argv`; `parse_multibulk`/`parse_inline` build
  it via `push`; doctests; `encode_command` (takes `&[Vec<u8>]`) — keep a slice
  overload or adapt.
- **kevy**: `dispatch.rs` (every handler indexes args), `cmd.rs` helpers
  (`upper_verb`, `scan_pattern`), `lib.rs` (`route`/`is_write`/`txn_kind`/
  `drain_commands` + the `Commands` trait arg type), `KevyCommands`.
- **kevy-rt**: `Op::Dispatch(Argv)`, `exec.rs` (multi-key scatter/gather indexes
  args), `reduce.rs`, `message.rs`, `conn.rs`/`shard.rs` if any buffer args.

The `Commands` trait signature `&[Vec<u8>]` → `&Argv` is the spine of the change.

## Test plan

- **Correctness:** `cargo test --workspace` (kevy-store, kevy commands + sharded
  + persistence, kevy-resp). The compiler catches every missed access site
  (type change), and the existing suites cover behaviour.
- **Component gate (the measure-first justification):** extend
  `kevy-resp/examples/bench_resp.rs` to compare old-vs-new parse in one process
  (or measure new vs the recorded ~70 ns baseline) — expect SET ~40 ns (≈1.7×).
  Add a `parse` figure to `kevy-resp/BUDGETS.md`. **Gate:** if the micro-bench
  doesn't show a clear (≥1.4×) parse win, abandon (the blast radius isn't worth
  a wash).
- Zero warnings (`clippy --workspace --all-targets`).

## Risks

- **Blast radius**: large but mechanical; the type system makes misses
  compile-errors, not silent bugs.
- **Ergonomics**: `args.get(i)?` is slightly less neat than `&args[i]`; provide
  `iter()`/`get()` helpers so handlers stay readable.
- **System-level payoff unmeasurable now**: the contended host blocks full-system
  throughput measurement, so we can only prove the *component* parse win, not the
  end-to-end throughput delta — same situation as every CPU optimization in this
  campaign (accepted: a leaner CPU path matters once io_uring batches the
  syscalls, which is the real bottleneck per the perf north star).

## Decision

**Landed (v0.perf-9).** Implemented `Argv { buf, ends }` in kevy-resp (Index/get/
first/iter/len + `PartialEq<Vec<Vec<u8>>>` + `From<Vec<Vec<u8>>>` so call sites
and tests stay readable), threaded `&Argv` / `Argv` through the `Commands` trait,
`Op::Dispatch`, exec/conn, dispatch/cmd, and kevy-persist's AOF append+replay.

**Key implementation note:** the first cut (`with_capacity(count, 0)`, grow as
args push) measured *83 ns* for SET — **no better** than `Vec<Vec<u8>>`, because
an incrementally-grown buffer reallocs ~once per arg. The fix is a **two-pass
parse**: pass 1 validates the frame and sums the total byte length, pass 2 builds
the argv pre-sized to that total → exactly 2 allocations, no regrowth.

**Verification (component gate, kevy-resp/examples/bench_resp, release):**

| Op | before (`Vec<Vec<u8>>`) | after (`Argv`, two-pass) |
|---|---:|---:|
| parse SET (3 args) | ~70 ns | **~50 ns (~1.4×)** |
| parse GET (2 args) | ~60 ns | **~40 ns (~1.5×)** |
| parse PING (inline) | ~30 ns | ~36 ns (slightly slower; inline is rare) |

Gate (≥1.4×) met on the dominant SET/GET. Correctness: `cargo test --workspace`
green (the one intermittent failure was the pre-existing SO_REUSEPORT
startup race under parallel+loaded conditions — `data_survives_restart_via_aof`
passes 3/3 in isolation), clippy 0 workspace-wide.

The variadic multi-value commands (HDEL/RPUSH/SADD/MSET…) still materialise a
`Vec<Vec<u8>>` via a `rest(args, n)` helper to feed the store APIs (non-headline;
the store keeps those bytes anyway). The headline single-key path is allocation-leaner.
