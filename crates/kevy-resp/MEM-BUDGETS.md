# Memory budgets — kevy-resp

## Per-op heap allocations

| Operation                              | Allocations | Source |
|----------------------------------------|-------------|--------|
| `parse_command_into(buf, scratch)`     | **0** (warmed) | reuses caller-owned `Argv` buffer; `Argv::clear` keeps capacity. |
| `parse_command(buf)` (legacy)          | 2 per call  | one for `Argv::buf`, one for `Argv::ends`. Use `parse_command_into` on the hot path. |
| `parse_reply(buf)` — `Reply::Simple`/`Error`/`Bulk` | 1 | `Vec<u8>` for the payload. |
| `parse_reply(buf)` — `Reply::Int`/`Nil`| **0**       | scalar / unit variants. |
| `parse_reply(buf)` — `Reply::Array(n)` | 1 (capacity-capped) | DoS fix: `min(count, remaining_bytes)`. |
| `encode_simple_string` / `encode_error` / `encode_integer` / `encode_bulk` / `encode_null_bulk` / `encode_array_len` | **0** (caller-owned `Vec<u8>` reused) | append-only into the caller's buffer. |

## Stack footprint

| Type      | `size_of`                         |
|-----------|-----------------------------------|
| `Argv`    | 48 B (two `Vec`s)                 |
| `Reply`   | 32 B (largest variant: `Bulk(Vec<u8>)`)  |

`Argv` is `Send`, so the thread-per-core runtime can forward it by value
across cores. The two `Vec`s keep their capacity across `Argv::clear`, so
hot-path malloc rate ≈ 0 once warmed.

## Verifying live

```bash
cargo run --release -p kevy-resp --example bench_resp
cargo test --release -p kevy-resp --test perf_gate
```

## DoS-resistance contract

`parse_array_reply` previously trusted the wire `count` to size its
`Vec::with_capacity`. The fix caps capacity to remaining buffer bytes:

```rust
let cap = (count as usize).min(buf.len().saturating_sub(pos));
let mut items = Vec::with_capacity(cap);
```

This bounds peak allocation to `O(bytes_remaining_in_buf)` regardless of
what the wire claims. Test coverage in `tests/dos.rs`; corpus seeded for
fuzz at `fuzz/corpus/parse_reply/`.
