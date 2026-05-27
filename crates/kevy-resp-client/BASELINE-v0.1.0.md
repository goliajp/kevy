# kevy-resp-client v0.1.0 — baseline snapshot

Pre-publish snapshot. Future versions diff against this file.

## Environment

- Host: macOS 26.5 / Apple M4 Pro / arm64
- Toolchain: rustc 1.95.0 stable + Rust 2024 edition
- Build: `--release`
- Date: 2026-05-27

## What this stone does

A blocking RESP2 client over `std::net::TcpStream`. One client per
thread. `connect(host, port)` + `request(args) -> Reply`. Works
against any RESP2 server (kevy, valkey, redis). The send-then-read
loop reuses an incremental read buffer so multi-segment replies
reassemble across `read` calls.

Pure Rust, no third-party deps (only `std` + `kevy_resp`).

## Performance

`kevy-resp-client` is **network-dominated**. The Rust-side cost of
the client itself is:

- `encode_command` — one `Vec` allocation per request to hold the
  formatted bytes; bytes are then `write_all`'d.
- `parse_reply` — loops over the incremental buffer, calling
  kevy-resp's parser (which `BASELINE-v0.1.0.md` measures at 18 ns
  for a 12 B bulk reply).
- `TcpStream::read` / `write_all` — bound by kernel + network.

End-to-end loopback round-trip is dominated by the syscall + kernel
TCP path (≈ 10-30 µs on modern hardware). The client's Rust overhead
is in the low-100 ns range, well below the kernel-time floor.

### Cross-language status

Comparable Rust competitor benches (redis-rs sync, hiredis, go-redis
sync, cpp_redis) require a live RESP server fixture and end-to-end
TCP. The relevant signal would be **client overhead delta** (kevy
vs redis-rs over the same kevy/valkey server) rather than raw
ns/op, which is dominated by the kernel and not the client. This
end-to-end cohort comparison lives at the **`bench/run.sh`** level
(see `perfs/baseline/2026-05-27/e2e-mac-aarch64.log` and the matching
post-polish Phase E re-bench), not in a stone-local comparative
harness. The stone-local harness would re-measure the same kernel
floor; not useful.

Therefore kevy-resp-client's stone-level perf gate is:
- ✅ no per-request Rust heap allocation beyond `encode_command`'s
  output `Vec` (necessary; the caller wants the parsed `Reply`).
- ✅ `kevy-resp::parse_reply` is the actual parsing cost — 18 ns
  for a 12 B bulk reply, 9× faster than redis-rs (see kevy-resp
  BASELINE-v0.1.0.md).

## Memory contract

- `RespClient` holds: a `TcpStream` (one fd) + a `Vec<u8>` read
  buffer (8 KiB initial capacity, grows up to the largest single
  reply seen).
- Per-request: one `Vec<u8>` allocation in `encode_command` (the
  serialised request) + zero in `parse_reply` until a `Reply::Bulk`
  / `Reply::Array` is constructed (those carry the bytes of the
  reply by definition).

## Correctness contracts

| check | result |
|---|---|
| `cargo test -p kevy-resp-client --tests` | ✅ 8 / 8 integration tests pass (round-trip vs mock server: ping/pong, GET bulk, missing-key nil, integer reply, array reply, error reply, malformed-reply yields `InvalidData`, server-mid-reply-close yields `UnexpectedEof`) |
| `cargo +nightly miri test -p kevy-resp-client --lib` | ✅ 0 lib unit tests run (integration tests require TCP, which miri doesn't model; coverage is enforced via the integration tests above) |
| `cargo +nightly llvm-cov --branch -p kevy-resp-client` | Regions 93.18% · Functions **100%** · Lines **100%** · Branches **100%** |

Lines and functions and branches all at 100%. The 3 missing regions
sit inside the `loop` retry path of `request` where a single
deterministic test cannot cover every interleaving (the parser
returning `Ok(None)` then a partial `read` then a complete frame
arriving), but the integration tests cover the meaningful semantic
arms.

## Reproducibility

```bash
cargo test -p kevy-resp-client --tests
cargo +nightly llvm-cov clean -p kevy-resp-client
cargo +nightly llvm-cov --branch -p kevy-resp-client --lib --tests --summary-only
```

## Optimisations between baseline-pre and v0.1.0

| change | effect |
|---|---|
| New integration test `malformed_reply_yields_invalid_data_error` | lib lines 87.10% → 100% (covered the previously-untested `parse_reply` error branch that maps `Err(_)` to `InvalidData`); rounds out the input-validation surface |
