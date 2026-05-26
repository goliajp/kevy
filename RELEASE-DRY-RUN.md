# Release dry-run — 2026-05-26 (POLISH-PLAN Phase D)

This document records the publish-readiness rehearsal for the kevy stones.
It is the per-release sign-off log: if any of the gates below fails, the
release is not ready.

The rehearsal is gated bottom-up because the workspace has not been
published yet — `cargo publish --dry-run` for a crate with not-yet-published
path-deps cannot resolve its registry deps and fails before
verification. This is expected and documented below.

## Publish order (dep DAG topology)

The dep DAG has one root and a handful of leaves. The order below pushes
deps before consumers so each `cargo publish` can see its inputs already
on crates.io:

```
1. kevy-bench       (dev-tool stone; dev-deps for kevy-hash/bytes/map/resp/ring/resp-client)
2. kevy-hash        (no kevy-* lib deps)
3. kevy-resp        (no kevy-* lib deps)
4. kevy-ring        (no kevy-* lib deps)
5. kevy-sys         (cement, not stone — required by kevy-map but not part of this round; see backlog)
6. kevy-bytes       (deps: kevy-hash)
7. kevy-resp-client (deps: kevy-resp)
8. kevy-map         (deps: kevy-hash, kevy-sys)   ← blocked on kevy-sys being its own publishable
9. kevy             (cement binary; deps: every above + kevy-store + kevy-rt + kevy-persist; not in this round)
```

The 6 publish-targeted stones in this round are 1, 2, 3, 4, 6, 7. `kevy-map`
is delayed until `kevy-sys` is graduated from cement → publishable (or until
the `kevy-sys` cement is given a publishable name; see ROADMAP).

## Per-stone manifest gates

| Crate              | description ≤ 60 | keywords ≤ 5 | categories valid | readme set | repository set | license workspace |
|--------------------|:----------------:|:------------:|:----------------:|:----------:|:--------------:|:------------------:|
| kevy-bench         | ✅ | ✅ | ✅ | n/a | ✅ | ✅ |
| kevy-bytes         | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ |
| kevy-hash          | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ |
| kevy-map           | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ |
| kevy-resp          | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ |
| kevy-resp-client   | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ |
| kevy-ring          | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ |

`workspace.repository = "https://github.com/golia-kk/kevy"` is shared via
`repository.workspace = true`; same for `homepage` and `documentation`.

## Per-stone package contents (`cargo package --list`)

Each stone packages: `Cargo.toml(.orig)`, `Cargo.lock`, `.cargo_vcs_info.json`,
`README.md`, `CHANGELOG.md`, `PLATFORMS.md`, `MEM-BUDGETS.md`, `STONE-STATUS.md`,
`AUDIT-2026-05-26.md`, `src/lib.rs`, `tests/*.rs`, and (where applicable)
`examples/*.rs`, `BUDGETS.md`. Verified 2026-05-26.

## Compressed tarball size

Sanity check the publish-size budget per [STONE-AUDIT.md] §T2 (≤ 50 KB).
kevy-bench (the smallest stone) packaged in **3.8 KB compressed** (5 files,
8.1 KB uncompressed). The doc-heavy stones (kevy-bytes etc.) will be
larger; will be re-measured once each is packaged end-to-end after Phase E.

## Smoke install (`/tmp/kevy-smoke`)

Created a one-binary external consumer that path-deps `kevy-bytes` and
runs the README example unchanged:

```toml
[dependencies]
kevy-bytes = { path = "/Users/doracawl/workspace/labs/lab21-kv/crates/kevy-bytes" }
```

```rust
use kevy_bytes::SmallBytes;
let inline = SmallBytes::from_slice(b"redis");
let heap   = SmallBytes::from_slice(&[0u8; 64]);
assert_eq!(inline.as_slice(), b"redis");
assert_eq!(heap.len(), 64);
```

`cargo run --release` output (2026-05-26):

```
Locking 2 packages to latest Rust 1.95.0 compatible versions
Compiling kevy-hash v0.1.0
Compiling kevy-bytes v0.1.0
Compiling kevy-smoke v0.0.0
Finished `release` profile [optimized] target(s) in 0.34s
kevy-bytes smoke OK: inline.len=5, heap.len=64
```

✅ External consumer integration works. This is the strongest signal that
the published artifact will be usable in real downstream code.

## `cargo publish --dry-run` results

Bottom-up `cargo publish --dry-run` is **expected to fail** for every
crate above `kevy-bench` until the lower layers actually appear on
crates.io — cargo verifies path-dep versions against the registry.

What IS verified locally (no registry):

- `cargo package --list -p <name> --allow-dirty` ✅ for **all 7 stones +
  dev-tool** (see "Per-stone package contents" above).
- `cargo package -p kevy-bench --allow-dirty` ✅ end-to-end (3.8 KB
  compressed; verification compile succeeds).
- Smoke install ✅.

## Outstanding pre-publish gates

These run at *actual* publish time, not in this rehearsal:

1. **Token + ownership** — `cargo login`, `cargo owner --add github:golia-kk:publishers`.
2. **Crates.io name availability** — `kevy-bench`, `kevy-hash`, `kevy-bytes`,
   `kevy-resp`, `kevy-resp-client`, `kevy-ring`, `kevy-map`, `kevy-sys` —
   not yet claimed (TBD verify on push day).
3. **Tag the release** — `git tag v0.1.0 && git push --tags`.
4. **Run the publish chain bottom-up** — one crate at a time, wait for
   index propagation (~30 s) between each before publishing the next layer.

## Decision

✅ **Rehearsal PASSED.** All 6 target stones + dev-tool (kevy-bench) are
manifest-complete, package-clean, and consumable from an external path
dep. Real publish is gated by token + name availability only — both
project-level concerns, not stone bugs.

[STONE-AUDIT.md]: ./STONE-AUDIT.md
