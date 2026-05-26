# kevy-uring — supported platforms

| Target                       | Status   | Notes                                          |
|------------------------------|----------|------------------------------------------------|
| `x86_64-unknown-linux-gnu`   | Tier 1   | Primary. Validated on metal-perf harness.      |
| `aarch64-unknown-linux-gnu`  | Tier 1   | Same kernel ABI; `io_uring_setup` syscall #s identical across Linux architectures. |
| `x86_64-apple-darwin`        | Tier 2   | Crate compiles to an empty module; calling sites must `cfg(target_os = "linux")`. |
| `aarch64-apple-darwin`       | Tier 2   | Same — empty crate.                            |
| Anything else                | Tier 2   | Same — empty crate on every non-Linux target.  |

**Tier definitions** (mirrors rustc usage)

- **Tier 1** — the engine is fully exposed; integration tests
  exercise NOP / read / batched / accept / echo / multishot+provided-
  buffer.
- **Tier 2** — the crate is empty (`#![cfg(target_os = "linux")]` zeroes
  the whole library). Callers do not need to write per-target imports;
  they just `cfg`-gate the call sites.

## Minimum kernel

- `IoUring::new` + `prep_nop` / `prep_read` / `prep_accept` / `prep_write`:
  Linux 5.4+ (basic io_uring landed in 5.1; we use the `IORING_OP_ACCEPT`
  opcode which is 5.4+).
- `prep_recv_multishot`: Linux 5.19+ (multishot RECV + `IORING_RECV_MULTISHOT`).
- `register_buf_ring` (provided-buffer ring): Linux 5.19+.

Tests that need a multishot/PBR feature `ring_or_skip` out if the kernel
returns ENOSYS / EINVAL, so the suite runs cleanly on older kernels with
the advanced features simply skipped.

## Container note

Docker's default seccomp profile blocks `io_uring_setup` →
`EPERM/ENOSYS`. Run with `--security-opt seccomp=unconfined` so the
engine is reachable; otherwise tests SKIP rather than fail.
