# Platform support — kevy-ring

Uses `unsafe` for the `UnsafeCell` + atomic-based ring layout. No C, no
FFI, no platform-specific syscalls.

## Hard requirements

| Requirement       | Detail |
|-------------------|--------|
| Atomics           | Target must support `core::sync::atomic::AtomicUsize` Release/Acquire ordering. All mainstream architectures qualify. |
| Cache-line size   | Assumes 64-byte cache lines for the head/tail padding. Larger cache lines waste a few bytes per ring; smaller "false sharing" risk is mitigated by the padding (still upper-bound-correct). |
| Pointer width     | 32-bit or 64-bit. |
| Rust toolchain    | 1.95+. |

## Tested targets

| Target                       | Status | Notes |
|------------------------------|--------|-------|
| `aarch64-apple-darwin`       | ✅ daily | Primary dev host. 7/7 cross-thread tests under miri. |
| `x86_64-unknown-linux-gnu`   | ✅ daily | Primary deploy target. |

## Untested but expected to work

`aarch64-unknown-linux-gnu`, `x86_64-apple-darwin`, `riscv64-unknown-linux-gnu`
(any 64-bit target with Acquire/Release atomics).

## Architecture-specific notes

- **Apple silicon (aarch64-apple-darwin)**: cross-thread ring item cost is
  ~63 ns (vs 6–10 ns on lx64 x86_64) — Apple's snoop-based coherence is
  expensive across performance/efficiency core boundaries. This is a
  platform property, not a crate cost.
- **x86_64 with TSO**: Release/Acquire pair compiles to plain MOV +
  `MFENCE`-free path on most paths. The cost is the cache-line transfer,
  not the fence.
