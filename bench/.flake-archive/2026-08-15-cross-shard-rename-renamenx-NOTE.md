# `cross_shard_rename_survives_a_restart` — first occurrence

CI run 31834491043, job `test (x86_64-unknown-linux-gnu)`, on
`feature/luna-core-3` at `9a41cb28`.

```
---- cross_shard_rename_survives_a_restart stdout ----
assertion `left == right` failed: expected ":0\r\n"
  left: [58, 49, 13, 10]     // :1
 right: [58, 48, 13, 10]     // :0
```

The assertion is `RENAMENX keep taken` → `:0`, the refusal branch: `keep`
and `taken` are on different shards, `taken` was just written, so the
rename must decline. It returned `:1` — as if `taken` did not exist.

## Why it is not the luna bump

The commit is a dependency bump of the Lua runtime (luna-core 2.16.0 →
3.0.0) plus documentation. `RENAMENX` does not go near the Lua bridge,
and no test on this path reads a Lua reply. The Lua suite itself is
green (128 tests).

## Reproduction attempts — none succeeded

| where | shape | runs | failures |
|---|---|---|---|
| lx64 (Linux, same OS as CI) | the test alone, quiet machine | 60 | 0 |
| lx64 | the test alone, under 12-way CPU contention | 60 | 0 |
| lx64 | `cargo test --workspace --lib --tests` — CI's concurrency shape | 3 | 0 |
| macOS (kqueue) | the test alone | 25 | 0 |
| CI, same commit, re-run of the failed job | | 1 | 0 |

macOS is kqueue where CI is epoll, so the local runs are not exoneration
on their own — the Linux ones are the load-bearing rows.

## An unproven hypothesis worth writing down

`read_reply` in `crates/kevy/tests/persistence.rs` reads exactly as many
bytes as the reply it expects:

```rust
let mut buf = vec![0u8; expected.len()];
s.read_exact(&mut buf).unwrap();
```

That is robust against segmentation, and fragile against a reply that is
*longer* than expected: the surplus stays in the socket, and every read
after it is shifted. A failure would then be reported at a command
downstream of the one that actually went wrong, with a value that is a
fragment of an earlier reply — which is the shape of what CI saw (`:1`
is a legitimate reply to several of the earlier commands).

This is a hypothesis, not a finding: nothing here establishes that a
longer reply occurred. It is recorded because it is a real weakness in
the harness independent of this failure — a mismatch should be reported
where it happens, not three commands later. Fixing it means reading a
whole RESP reply and comparing that, which is test-only work and does
not belong on a release branch.

## Verdict

Archived as a first occurrence. If it recurs, start from the hypothesis
above: instrument `read_reply` to drain and report the full reply on
mismatch, and re-read this note before assuming the engine.
