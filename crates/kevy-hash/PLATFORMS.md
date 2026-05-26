# Platform support — kevy-hash

`kevy-hash` is pure Rust, `#![forbid(unsafe_code)]`-friendly, with no C, no
FFI, no platform-specific syscalls.

## Hard requirements

| Requirement       | Detail |
|-------------------|--------|
| Endianness        | Any (algorithm is byte-stream — no endian-dependent loads). |
| Pointer width     | 32-bit or 64-bit (no `usize` math beyond `len()`). |
| Rust toolchain    | 1.95+ (uses `edition = 2024`). |

## Tested targets

| Target                       | Status | Notes |
|------------------------------|--------|-------|
| `aarch64-apple-darwin`       | ✅ daily | Primary dev host. |
| `x86_64-unknown-linux-gnu`   | ✅ daily | Primary deploy target (lx64). |

## Untested but expected to work

`aarch64-unknown-linux-gnu`, `x86_64-apple-darwin`, `wasm32-*` (no FFI),
`thumbv7em-none-eabi` (no `std` required for the core hash).

`no_std`-compatibility is not currently asserted by a CI target but the
crate only uses `core::hash`. A future point release may add a `no_std`
feature gate.

## Trust-domain note

This hasher is **not DoS-resistant**. Do not use across a trust boundary.
For untrusted input, layer a per-shard rate-limit or use SipHash. kevy's
single-process design means one shard owns one trust domain.
