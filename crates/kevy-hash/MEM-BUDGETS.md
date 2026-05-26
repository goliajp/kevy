# Memory budgets — kevy-hash

The hasher does no heap allocation on any path. This file pins that
contract so caller-side budgets can rely on it.

## Per-op heap allocations

| Operation                         | Allocations | Source |
|-----------------------------------|-------------|--------|
| `FxFmixHasher::write(&[u8])`      | **0**       | reads bytes into `u64` words by `read_unaligned`, XOR-mixes into the state. |
| `FxFmixHasher::finish()`          | **0**       | applies `fmix64` to the state and returns. |
| `kevy_hash::kevy_hash(b: &[u8])`  | **0**       | stateless helper — single pass, no Hasher state machine. |
| `FxFmixBuildHasher::build_hasher`| **0**       | returns a zero-state `FxFmixHasher`. |

## Stack footprint

| Type                  | `size_of` |
|-----------------------|----------:|
| `FxFmixHasher`        | 8 B (a single `u64` of state) |
| `FxFmixBuildHasher`   | 0 B (ZST) |

The hasher state is one `u64`; the BuildHasher is a ZST. Drop-in use with
`HashMap<K, V, FxFmixBuildHasher>` adds zero per-bucket overhead vs the
default.

## Verifying live

```bash
cargo run --release -p kevy-hash --example bench_hash
cargo test --release -p kevy-hash --test perf_gate
```

Per-op ns numbers and ratios are in [`BUDGETS.md`](./BUDGETS.md).
