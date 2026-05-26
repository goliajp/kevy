# kevy-resp-client — publish status

Snapshot of the verification matrix as of the most recent audit.

## Latest

- **Version**: 0.1.0
- **Audit**: [`AUDIT-2026-05-26.md`](./AUDIT-2026-05-26.md) (T1 PASS, T2 PASS)

## Test runs

| Tool / target              | Toolchain                          | Timestamp     | Result        |
|----------------------------|------------------------------------|---------------|---------------|
| `cargo test`               | stable 1.95                        | 2026-05-26    | 7/7 PASS      |
| `cargo clippy -Dwarnings`  | stable 1.95                        | 2026-05-26    | 0 findings    |
| `cargo doc`                | stable 1.95                        | 2026-05-26    | 0 warnings    |
| `cargo-llvm-cov` line cov  | stable 1.95                        | 2026-05-26    | 87.10%        |

## Miri

Network I/O (`std::net::TcpStream`) is **not supported under miri**, so
the integration tests cannot be miri'd directly. The codec layer it
depends on (`kevy-resp`) is `#![forbid(unsafe_code)]` and miri-clean;
this crate adds only the connect-and-buffer wrapper.

## Fuzz

Not directly applicable — this crate is the I/O wrapper. Wire-format
fuzz lives in `crates/kevy-resp/fuzz/`.

## Known issues

None.
