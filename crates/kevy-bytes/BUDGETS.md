# kevy-bytes performance budgets

Per-op ns numbers + reproducer commands. SmallBytes' main contract is the
**inline path** (≤22 B fits in the value, no allocation, no pointer chase);
the heap path is `Vec<u8>` parity.

## Reproducer

```bash
cargo run --release -p kevy-bytes --example bench_sso
cargo test --release -p kevy-bytes --test perf_gate
```

## Layout contracts (compile-time)

`size_of::<SmallBytes>() == 24` and `align_of::<SmallBytes>() == align_of::<usize>()`
are asserted by a `const _: () = { assert!(...) };` block at lib.rs (won't
even compile if violated). The `tests/perf_gate.rs::size_and_align_pinned`
test is a runtime fallback for cross-compile validation.

## Budget gates (perf_gate.rs)

Tests assert per-op median stays under a conservative ceiling.

| Test                                  | Ceiling     | What it gates |
|---------------------------------------|-------------|---------------|
| `from_slice_inline_under_budget`      |  50 ns/op   | 12B inline build = memcpy + tag write |
| `clone_inline_under_budget`           |  50 ns/op   | 24-byte memcpy + tag copy |
| `as_slice_inline_under_budget`        |  20 ns/op   | load tag + range slice |

Current readings (mac aarch64, loaded host): from_slice ~10-15 ns, clone
~5-10 ns, as_slice ~2-5 ns. Ceilings give 3-10× headroom for noise.

## TODO

- Alloc-count test (swap a `GlobalAlloc` that counts; assert inline path
  has 0 allocations on a tight loop)
- Heap-path baseline numbers (currently only inline path is gated)
- lx64 x86_64 numbers + ratio vs `Vec<u8>` on production target
