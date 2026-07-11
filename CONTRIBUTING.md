# Contributing to kevy

Thanks for your interest in kevy! This document covers the ground rules
for code contributions. They are few, but they are enforced — most of
them mechanically, by the same CI gates that run on every push.

## Ground rules

### Pure Rust, zero dependencies

kevy is pure Rust with **no third-party crates**: `std` plus the
workspace's own `kevy-*` crates only (the single carved exemption is
`luna-core`, GOLIA's own zero-dependency Lua interpreter, used by
`kevy-lua`). Algorithms and data structures — hashing, maps, protocol
parsing, the reactor — are implemented in-tree. The only permitted OS
boundary is hand-written `unsafe extern "C"` bindings, centralized in
`kevy-sys` (sockets, pollers, mmap, time). PRs that add a dependency,
or add FFI outside `kevy-sys`, will be declined regardless of quality.

### Comments describe code, not history

A comment states the **semantics and constraints** of the code in
front of the reader: invariants, edge cases, the reasoning behind a
non-obvious shape. It does not narrate how the code got here — no
version numbers, no dates, no references to internal plans or past
iterations. History lives in `git log` and the CHANGELOG, where it is
maintained once and cannot drift from the code.

```rust
// Not this:
//   v2.3 (2026-05-01): re-queue if more work remains.
// This:
//   A chunked writev may leave bytes in `output`; re-queue the conn
//   so the next arm visit re-preps the send SQE.
```

Every `unsafe` block needs a `// SAFETY:` line explaining why its
preconditions hold at this call site.

### API conventions

- **Constructors**: local resources use `open` (stores, embedded
  engines), network endpoints use `connect` (clients). `new` is
  reserved for infallible in-memory values.
- **Errors**: the public error currency is `KevyError`. Don't downgrade
  structured errors to `io::Error::other`, and don't use strings as
  error types across a public boundary.
- **Builders**: constructors with more than two optional knobs take a
  builder (field-named methods, consuming `self`, `build()` at the
  end) rather than `new_with_…` permutations.
- **`#[must_use]`** on pure queries and on returns that are
  meaningless unless consumed.
- Keep public surfaces small: a crate should expose a handful of
  entry points, with internals behind `pub(crate)`.

### Size limits

Source files are capped at **500 lines** and functions at **50 lines**
(`bench/locgate.sh` enforces this in CI). Test files are exempt. The
only sanctioned waiver is a pure data-driven dispatch/match table,
marked with a `// LOC-WAIVER:` comment. If your change pushes a file
near the cap, split it by responsibility — don't wait.

## Testing and gates

kevy's quality bar is expressed as executable gates, not review
opinions. Locally:

```sh
cargo test --workspace          # unit + integration tests
cargo clippy --workspace --all-targets -- -D warnings
bash bench/locgate.sh           # size limits
bash bench/commentgate.sh       # comment hygiene
```

CI additionally runs the coverage ratchet (`covgate`), miri on the
unsafe cores, wasm32 target checks, byte-for-byte reply differentials
against valkey 9.1 and redis 7.4 (`compat3`), and the behavior
contract gates (availability, replication, MCP). New functionality is
expected to come with tests that would fail without it; changes to
performance-sensitive paths should mention how they were measured.

## Pull requests

- Target the `develop` branch.
- **All gates green** — a PR with a red check is not reviewable yet.
  Gates are ratchets: they may tighten in your favor, never loosen.
- Keep commits in the imperative `type: summary` style used in the
  history (`fix: …`, `refactor: …`, `docs: …`).
- One concern per PR. Refactors and behavior changes travel separately.
- Breaking API changes are only possible in a major-version window;
  outside one, additive changes only.

## License

By contributing, you agree that your contributions are dual-licensed
under MIT OR Apache-2.0, matching the project.
