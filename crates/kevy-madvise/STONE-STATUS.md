# kevy-madvise — publish status

Snapshot of the verification matrix as of the most recent audit.

## Latest

- **Version**: 0.1.0
- **Audit**: [`AUDIT-2026-05-27.md`](./AUDIT-2026-05-27.md) (T1 PASS, T2 PASS, T3 ready)

## Test runs

| Tool / target              | Toolchain                          | Timestamp     | Result        |
|----------------------------|------------------------------------|---------------|---------------|
| `cargo test`               | stable 1.95                        | 2026-05-27    | 4 / 4 PASS    |
| `cargo clippy -Dwarnings`  | stable 1.95                        | 2026-05-27    | 0 findings    |
| `cargo doc`                | stable 1.95                        | 2026-05-27    | 0 warnings    |

## Fuzz

Not directly applicable — single function, no parser, no untrusted input.
The wrapper rounds the request to page boundaries and silently no-ops on
kernel `EINVAL`; the unit tests cover unaligned / zero-length / under-
threshold and large-aligned regions.

## Known issues

None. Carved out of `kevy-sys` per `KEVY-SYS-VERDICT-2026-05-27.md` — no
pre-existing bugs migrated with the move.
