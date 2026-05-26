# kevy-ring performance budgets

Bounded SPSC ring with cache-line-padded head/tail (avoids false sharing).
Push and pop are O(1); cross-thread path costs one cache-line transfer when
producer and consumer are on different cores.

## Reproducer

```bash
cargo run --release -p kevy-ring --example bench_ring
cargo test --release -p kevy-ring --test perf_gate
```

## Bench numbers

`examples/bench_ring.rs` captures same-thread push+pop AND cross-thread
SPSC round-trip at cap=256 and cap=1024. Headline data from the
v0.metal-1 baseline (lx64 x86_64, see `perfs/data/2026-05-26/metal-baseline.txt`):

| Path                        | cap  | ns/item | items/sec |
|-----------------------------|-----:|--------:|----------:|
| Same-thread push+pop        |   —  |    ~1   |       —   |
| Cross-thread SPSC (lx64 x86_64) |  256 |   9.5   | 104.8M    |
| Cross-thread SPSC (lx64 x86_64) | 1024 |   6.1   | 165M      |

Cross-thread item cost on **mac aarch64** is ~63 ns — Apple silicon's
coherence cost is much higher than x86, but kevy's production target is
lx64 so the headline lock-free hop cost is 6-10 ns/item.

## Budget gates (perf_gate.rs)

| Test                                  | Ceiling  | What it gates |
|---------------------------------------|----------|---------------|
| `push_pop_same_thread_under_budget`   | 80 ns/op | same-thread push+pop pair |
| `capacity_is_power_of_two`            | n/a      | layout contract: cap is rounded UP to 2^n for the `& mask` index path |

## Memory ordering used

- `head` (consumer-side store): `Release` — publishes "I drained up to N".
- `tail` (producer-side store): `Release` — publishes "I produced up to N".
- Cross-side load of the other index: `Acquire` — pairs with the Release.
- Same-side load of own index: `Relaxed` — no ordering needed, exclusive owner.

This is the standard SPSC pattern; arguments + Loom-style reasoning live
in the lib.rs module doc.

## Tests covering cross-thread correctness

| Test                                  | Asserts |
|---------------------------------------|---------|
| `spsc_stress_across_threads`          | producer 0..=N, consumer reads same sequence, FIFO order |
| `stress_with_intermittent_consumer`   | producer waits when full; consumer waits when empty; both make progress |
| `drops_queued_elements_exactly_once`  | un-popped elements drop once at ring drop time |
| `wraps_around_many_times`             | index wraparound at usize::MAX boundary doesn't corrupt order |

Run under miri (`cargo +nightly miri test -p kevy-ring`) — passes 7/7 in
~19 minutes on mac aarch64. miri picks a single execution schedule, so it
validates the **memory ops are well-formed and that schedule is sound**;
it does NOT enumerate all schedules. For exhaustive interleaving
verification, `loom` would be the standard tool — DEFERRED in this audit
because kevy's charter is "0 crates.io deps", and loom is third-party.
If/when a real cross-thread bug requires it, loom can be added as a
cfg-gated dev-dep with explicit charter exception.

## Things this file does not yet cover

- Lx64 numbers re-captured under the latest commit (numbers above are
  from metal-1 baseline 2026-05-26 AM — kevy-ring hasn't changed since)
- MPMC variant (not on roadmap; the SPSC contract is the identity)
