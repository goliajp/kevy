# Platform support — kevy-resp-client

Pure Rust + `std::net::TcpStream`. No C, no FFI.

## Hard requirements

| Requirement       | Detail |
|-------------------|--------|
| `std`             | required (uses `std::net`, `std::io`). Not `no_std`-compatible. |
| Endianness        | Any. |
| Pointer width     | 32-bit or 64-bit. |
| Rust toolchain    | 1.95+. |

## Tested targets

| Target                       | Status | Notes |
|------------------------------|--------|-------|
| `aarch64-apple-darwin`       | ✅ daily | Primary dev host. 7/7 integration tests. |
| `x86_64-unknown-linux-gnu`   | ✅ daily | Primary deploy target. |

## Untested but expected to work

Anywhere `std::net::TcpStream` works: linux/macos/windows on x86_64 +
aarch64. `wasm32-wasip2` would need `tokio`-style async (not provided
here); this crate is intentionally blocking.

## Connection / I/O

- Single-shot `TcpStream::connect` (no retries, no exponential back-off
  — caller's policy).
- Blocking reads with a buffered reader; partial replies are accumulated
  until `parse_reply` produces a complete frame.
- Server-side mid-reply close surfaces as `io::ErrorKind::UnexpectedEof`.
