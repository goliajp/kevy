# kevy-uring — publish status

Snapshot of the verification matrix as of the most recent audit.

## Latest

- **Version**: 0.1.0
- **Audit**: [`AUDIT-2026-05-27.md`](./AUDIT-2026-05-27.md) (T1 PASS, T2 PASS, T3 ready)

## Test runs

| Tool / target              | Toolchain                          | Timestamp     | Result                              |
|----------------------------|------------------------------------|---------------|-------------------------------------|
| `cargo test` (Linux)       | stable 1.95                        | 2026-05-26    | 6 / 6 PASS (in kevy-sys::uring mod, prior to split) |
| `cargo test` (macOS)       | stable 1.95                        | 2026-05-27    | crate is Linux-only; 0 tests on Darwin (`#![cfg(target_os = "linux")]`) |
| `cargo clippy -Dwarnings`  | stable 1.95                        | 2026-05-27    | 0 findings                          |
| `cargo doc`                | stable 1.95                        | 2026-05-27    | 0 warnings                          |
| `cargo check`              | stable 1.95                        | 2026-05-27    | clean on both `aarch64-apple-darwin` and Linux target |

### Carry-over from kevy-sys (pre-split)

The Linux-side test suite (`nop_round_trips`, `reads_a_file`,
`batched_nops`, `accepts_a_connection`, `echo_round_trip_via_io_uring`,
`multishot_recv_with_provided_buffers`) was last green on 2026-05-26 in
the metal-perf harness when these tests still lived in `kevy-sys`. The
2026-05-27 split moved the file unchanged plus the test-helper migration
to `std::net::TcpListener`; semantic behavior is unchanged. To be
re-verified on a Linux host as part of the v0.publish chain.

## Fuzz

Not directly applicable — `kevy-uring` does not parse untrusted input.
It driver the kernel ring through documented head/tail cursors; the
kernel is the other side of the buffer, not an adversary. The protocol
parsers (`kevy-resp`) are where parser-fuzz pays off.

## Known issues

None. Carved out of `kevy-sys` per `KEVY-SYS-VERDICT-2026-05-27.md` — no
pre-existing bugs migrated with the move.
