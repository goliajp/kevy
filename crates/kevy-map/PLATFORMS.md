# Platform support — kevy-map

`kevy-map` uses `unsafe` for the bucket layout and exposes inline-asm
prefetch hints on x86_64 + aarch64. No C, no FFI; the prefetch path is
inline-asm gated by `cfg`.

## Hard requirements

| Requirement       | Detail |
|-------------------|--------|
| Endianness        | Little-endian. Bucket-byte layout assumes LE for the SIMD probe. |
| Pointer width     | 64-bit. |
| Rust toolchain    | 1.95+. |

## Tested targets

| Target                       | Status | Notes |
|------------------------------|--------|-------|
| `aarch64-apple-darwin`       | ✅ daily | Primary dev host. miri PASS. |
| `x86_64-unknown-linux-gnu`   | ✅ daily | Primary deploy target. |

## Prefetch fallbacks

- On x86_64: emits `_mm_prefetch` (T0 hint).
- On aarch64: emits `prfm pldl1keep`.
- Anywhere else (incl. under miri): semantic no-op. `prefetch_t0` is always
  safe to call; it just doesn't prefetch.

## Untested but expected to work

`x86_64-apple-darwin`, `aarch64-unknown-linux-gnu` — same probe path,
prefetch supported.

## Will not work

Big-endian targets — the SIMD probe encoding is LE.
