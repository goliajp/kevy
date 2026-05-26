# Platform support — kevy-bytes

`kevy-bytes` is pure Rust + `#![allow(unsafe_code)]` for the SSO union;
no C, no FFI, no platform-specific syscalls.

## Hard requirements

| Requirement              | Detail |
|--------------------------|--------|
| Endianness               | **Little-endian only.** `compile_error!` on BE targets (the tag byte is stored at the low-order byte of the union). |
| Pointer width            | 64-bit recommended; 32-bit untested. `size_of::<SmallBytes>() == 24` is asserted at compile time. |
| Rust toolchain           | 1.95+ (uses `edition = 2024`). |

## Tested targets

| Target                       | Status | Notes |
|------------------------------|--------|-------|
| `aarch64-apple-darwin`       | ✅ daily | Primary dev host. 17/17 tests + miri pass. |
| `x86_64-unknown-linux-gnu`   | ✅ daily | Primary deploy target (lx64). Same test suite. |

## Untested but expected to work

`aarch64-unknown-linux-gnu`, `x86_64-apple-darwin` — pure-Rust LE 64-bit; no
known portability hazards.

## Will not work

Big-endian targets (e.g. `mips-*`, `powerpc-*-eabi`) — guarded by
`compile_error!`.

32-bit targets — not asserted out, but the 24-byte layout assumes
`usize` is 8 bytes; the const assertion would trip.
