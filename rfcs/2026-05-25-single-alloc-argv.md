# RFC: single-allocation argv (`Command` representation)

**Status:** Draft — design + plan; implementation is its own focused checkpoint.

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

Queued as a deliberate checkpoint (v0.perf-9), not rushed at a session tail.
Implement behind the component gate above. If the parse micro-bench shows the
expected ~1.7×, land it; else drop and keep `Vec<Vec<u8>>`.
