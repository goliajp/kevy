# kevy-madvise — supported platforms

| Target                       | Status   | Notes                                          |
|------------------------------|----------|------------------------------------------------|
| `x86_64-unknown-linux-gnu`   | Tier 1   | `MADV_HUGEPAGE` issued via `madvise(2)`.       |
| `aarch64-unknown-linux-gnu`  | Tier 1   | Same syscall; same 4 KiB base-page rounding.   |
| `x86_64-apple-darwin`        | Tier 2   | Compile-time no-op (Darwin has no THP path).   |
| `aarch64-apple-darwin`       | Tier 2   | Compile-time no-op.                            |
| Anything else                | Tier 3   | Compile-time no-op (no `cfg`-gated FFI).       |

**Tier definitions** (mirrors rustc usage)

- **Tier 1** — the wrapper issues the syscall and is unit-tested against
  the kernel.
- **Tier 2** — the function compiles to a no-op; callers can write
  cross-platform code that calls `advise_hugepage` unconditionally.
- **Tier 3** — same as Tier 2 (the implementation gates Linux-vs-rest at
  compile time, so non-Linux is always a no-op without per-target work).

## Endian / page-size assumptions

- 4 KiB base-page rounding is the floor on every Linux target kevy
  supports. Systems that use 16 KiB or 64 KiB base pages would still
  satisfy the 4 KiB alignment (every multiple of those is also a
  multiple of 4 KiB), so the rounding is correct, just slightly more
  conservative than strictly necessary.
- No endian assumption — the wrapper passes an opaque pointer + length
  to the kernel.
