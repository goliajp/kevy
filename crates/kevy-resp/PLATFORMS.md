# Platform support — kevy-resp

Pure Rust, `#![forbid(unsafe_code)]`. No C, no FFI, no syscalls.

## Hard requirements

| Requirement       | Detail |
|-------------------|--------|
| Endianness        | Any. RESP is a byte protocol; no endian-dependent reads. |
| Pointer width     | 32-bit or 64-bit. |
| Rust toolchain    | 1.95+. |

## Tested targets

| Target                       | Status | Notes |
|------------------------------|--------|-------|
| `aarch64-apple-darwin`       | ✅ daily | Primary dev host. |
| `x86_64-unknown-linux-gnu`   | ✅ daily | Primary deploy target. |

## Untested but expected to work

`aarch64-unknown-linux-gnu`, `x86_64-apple-darwin`, `wasm32-*`. The crate
is `std`-only (uses `std::io::Result` only at the public surface for
error compatibility; the parser is `core`-clean otherwise).

## Notes

The SWAR'd `find_crlf` runs on any 64-bit target; on 32-bit it falls
back to a byte-at-a-time scan. Both are correctness-equivalent.
