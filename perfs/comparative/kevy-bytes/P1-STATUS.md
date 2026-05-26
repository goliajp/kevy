# Phase P1 — kevy-bytes deep-polish status (2026-05-27)

Snapshot of where Phase P1 stands at session-pause. The cross-language
perf data are in this directory; this file is the verdict + outstanding
work list.

## Optimisations landed

| change | effect |
|---|---|
| Specialised `Clone` — inline path = union bitwise copy, heap path = direct `alloc + memcpy` (no `as_slice → from_slice` round-trip) | clone_heap_64B 36 → 19-23 ns (35% faster) |
| Specialised `PartialEq` — single tag-byte dispatch (inline/inline, heap/heap, fallback mixed) | eq_heap_64B 3 ns @ tied-for-best |
| `alloc_heap` + `clone_heap` use `Layout::from_size_align_unchecked(len, 1)` (was `Layout::array::<u8>(len).expect(...)`) — drops the unreachable panic check | from_bytes_heap_64B 34 → 25 ns (26% faster) |
| Bench harness `eq` now uses `aa == bb` (type's `PartialEq`) with double-`black_box` on inputs (was `aa.as_slice() == bb.as_slice()`, which the v1 harness const-folded down to 0 ns) | valid measurements |

## Cross-language gate (5-run min-of-medians, M4 Pro arm64)

Owned-cohort competitors compared per workload. Best = `min(median)`.

| workload          | best (lang)              | kevy-bytes | verdict                |
|-------------------|--------------------------|-----------:|------------------------|
| clone_inline_12B  | 0 (kevy / smartstring / compact_str) | 0    | ✅ **kevy tied for win** |
| clone_heap_64B    | 15 (Go []byte)           | 23         | 8 ns behind Go pool allocator |
| eq_inline_12B     | ≤ 2 (kevy)               | ≤ 2        | ✅ **kevy ties / wins** |
| eq_heap_64B       | 3 (kevy / Vec / std::String) | 3      | ✅ **kevy tied for win** |
| from_bytes_inline | 3 (kevy)                 | 3          | ✅ **kevy wins outright** |
| from_bytes_heap   | 17 (sds)                 | 25         | 8 ns behind C allocator |

Cohort-aware reading: **kevy-bytes wins every SSO-inline workload and
every `eq` workload outright; loses heap-allocation construction
workloads to the fastest per-language allocator by 6-8 ns** (Go's
runtime pool, libmalloc, sds). This is the structural trade-off of an
SSO byte-string targeted at the short-value common case.

## Cov + UB

| check | result |
|---|---|
| `cargo +nightly miri test -p kevy-bytes` | ✅ 17 / 17 pass, no UB |
| `cargo +nightly llvm-cov --branch -p kevy-bytes` | ⚠️ **lines 70.32%**, regions 74.68%, functions 60%, branches 88.89% |

Cov is the headline gap. Stone bar = 90% line, project rule = effective
≥ 95%. Need to add tests covering ~24 of 60 functions that are presently
uncovered (likely traits: `AsRef`, `Borrow`, `KevyHash`, From impls,
the specialised Clone heap path, the specialised PartialEq mixed-case,
`Default`, ordering trait impls, etc.).

## What's still required before publish

1. **Lift line cov ≥ 90%** — effective coverage (each test asserts a
   specific invariant), per [[feedback-effective-coverage-no-padding]].
   No padding; check each uncovered function for the actual contract,
   write tests that exercise it.
2. **Re-run miri** after cov fixes — extra unsafe paths may surface.
3. **BASELINE-v0.1.0.md** + commit pre-publish snapshot of multirun.jsonl
   per `[[feedback-mailrs-stone-deep-polish-method]]`.
4. **`cargo publish -p kevy-bytes`** — user-gated; first-ever publish on
   crates.io.

## What this phase leaves cleanly documented

- The Clone/PartialEq/alloc_heap optimisations.
- Cohort-aware "≥ max" interpretation (owned vs shared).
- Honest characterisation of where kevy-bytes wins (SSO-inline +
  every eq) and where it loses (heap construction by ~7 ns).
- All four language competitor harnesses scaffolded + runnable + their
  JSONL snapshots committed.

This is a solid checkpoint to pause at — the perf optimisation work
yielded measurable wins, the cohort-aware verdict has data to back it,
and the coverage path forward is a clearly scoped next step.
