# FINDING 2026-08-27 — four published stones ship source files that cannot compile

**Status**: OPEN. Found by `tools/extract_stone.py` (v6 F1) on its first full
run; verified against the artifacts actually on crates.io, not against a
local `cargo package`.

## What it is

`kevy-bytes`, `kevy-hash`, `kevy-map` and `kevy-ring` each declare

```toml
[dev-dependencies]
kevy-bench = { path = "../kevy-bench" }
```

a **path-only dev-dependency with no version**. `kevy-bench` is `publish =
false` support scaffolding, so it is not on crates.io and cannot be.

`cargo package` resolves this by **dropping the dependency from the
published manifest** — and packages the source files that use it anyway.

## Verified against what is actually published

Downloaded from `static.crates.io`, version 5.4.1:

| crate | published size | files importing `kevy_bench` | declares it in the manifest |
|---|---:|---:|---:|
| kevy-map | 34,606 B | **2** | 0 |
| kevy-bytes | 22,641 B | **2** | 0 |
| kevy-hash | 15,571 B | **2** | 0 |
| kevy-ring | 17,103 B | **2** | 0 |
| kevy-geo *(control)* | 9,160 B | 0 | 0 |

The files are each crate's `tests/perf_gate.rs` and its `examples/bench_*.rs`.

## Blast radius, stated precisely

This does **not** break a dependent's build. Dev-dependencies and examples
are not compiled for a crate you depend on, so `cargo add kevy-map` and
`cargo build` are unaffected, and docs.rs builds only the lib.

It breaks the person who **takes the crate**: unpack it and run `cargo
test`, or `cargo build --examples`, and you get

```
error[E0432]: unresolved import `kevy_bench`
```

That is precisely the claim `suite/architecture.toml` makes about a stone —
"any project could take these" — so this is a defect against the stone
definition rather than a packaging nit.

## Why nothing caught it

Every existing check looks at the crate **inside** the workspace, where
`../kevy-bench` resolves. `cargo package --no-verify` does not build; the
verifying form builds the **lib**, which compiles fine. The failure only
exists in the published form, outside the workspace, at test time — a
place nothing looked until F1 unpacked a stone into a temp directory
outside the repository tree and ran its tests there.

## The two fixes, with a recommendation

**(a) Exclude the files that cannot stand alone.** `exclude = [...]` in each
manifest. Cheap and immediate, and it makes the shipped crate smaller. It
also means the published crate carries fewer tests than the repository does,
which weakens exactly the property F1 exists to check: take the stone, run
its tests.

**(b) Publish `kevy-bench`.** It is a benchmark harness with no business
knowledge — a stone by the project's own definition, currently classified
`support` because it was only ever needed in-tree. Publishing it and giving
the four dev-dependencies a version makes the shipped tests actually run.
Costs one more published crate and one more version to keep aligned across
the six layers.

**(b) is the better answer** and the one this finding recommends: the whole
point of the stone bar is that a stone arrives complete. Excluding the tests
to make the error go away is treating the symptom — the crate would still be
one a downstream user cannot exercise, it would just fail more quietly.

Either way the fix belongs to v6's stone work, not to the instrument that
found it. `extract_stone.py` reports; `stonegate` (G3) will hold the line
once the bar is set from data.

## Also recorded from the same run

`kevy-uring` lifts, builds and passes — with **zero tests**. A stone that
lifts and tests nothing satisfies "packaged, built, tested" while proving
almost nothing, so G3's bar needs a test-count floor, not just a boolean.

13 of 17 stones lift and pass their tests outside the workspace.
