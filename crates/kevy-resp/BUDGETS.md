# kevy-resp performance budgets

RESP2 wire-protocol codec — `parse_command`/`parse_reply` on the read side,
the `encode_*` family on the write side. The hot path is the server reactor
(kevy-rt) calling `parse_command_into` (the scratch-Argv form) once per cmd.

## Reproducer

```bash
cargo run --release -p kevy-resp --example bench_resp
cargo test --release -p kevy-resp --test perf_gate
```

## Path taxonomy

Every command crosses this codec twice: `parse_command` on the request, then a
reply encoder on the response. Both are on the per-command hot path.

- **`parse_command_into`** (metal-6 zero-alloc form) — once per request. Reuses
  a caller-owned `Argv` scratch buffer; per-cmd alloc rate ≈ 0 once warmed.
- **`parse_command`** — legacy convenience form that allocates a fresh Argv per
  call; kept for AOF replay + tests + non-hot callers.
- **reply encoders** (`encode_bulk`/`encode_simple_string`/`encode_integer`/…)
  — once per reply. They append into a caller-owned, reused buffer, so they
  allocate nothing themselves and are near-free.

## Bench numbers

`examples/bench_resp.rs` covers `parse_command(GET/SET/PING)` +
`encode_bulk/integer/simple_string`. The metal-1 baseline + metal-6/7 A/B
files in `perfs/data/2026-05-26/` are the authoritative production numbers
(this crate is on the per-cmd hot path; standalone microbench understates
the system effect).

Current per-flat-share on lx64 (after metal-7):
- `parse_command_into` ≈ 14% CPU
- `find_crlf` (SWAR'd) ≈ 9% CPU

## Regression budgets (`tests/perf_gate.rs`, dev profile, generous headroom)

| Path | Budget | Observed (dev, loaded) | Headroom |
|---|---:|---:|---:|
| `parse_command` SET | 10 µs | ~300 ns–1.5 µs | ~7–30× |
| `parse_command` inline | 5 µs | ~150 ns–800 ns | ~6–30× |
| `encode_bulk` | 5 µs | ~20–80 ns | large |

Loose by design — catches order-of-magnitude regressions (an accidental
O(n²) scan, a per-arg double-allocation), not ns drift. The tight
ns-budget gate lives in the server-end metal_keyspace.sh A/B.

## Fuzz coverage (added in v0.polish Phase A #4 audit)

`fuzz/fuzz_targets/parse_command.rs` + `fuzz/fuzz_targets/parse_reply.rs`
(libfuzzer-sys). Independent workspace at `fuzz/` so the third-party
fuzz infra is isolated from the published crate's 0-dep promise.

Run:
```bash
cargo +nightly fuzz run parse_command -- -max_total_time=3600
cargo +nightly fuzz run parse_reply   -- -max_total_time=3600
```

### Findings on smoke runs (60s each)
- **parse_command**: 45.9M executions, 0 crashes.
- **parse_reply**: initial 60s found a **real DoS bug** —
  `Vec::with_capacity(count as usize)` with a malformed `*999...\r\n`
  header (count = ~8e18) capacity-overflowed. Fixed inline (cap by
  remaining buffer bytes). Crash artifact retained in
  `fuzz/artifacts/parse_reply/crash-4c4ee6...` for regression coverage.
  After fix: 4.65M executions, 0 crashes.

Pre-publish gate per STONE-AUDIT.md §3.5 requires **≥ 1h on each target**
with 0 crashes. The 60s smoke runs are this audit's evidence; a
STONE-STATUS.md entry will record the formal 1h run before any publish.

## When to re-measure

- Changing the `Command`/`Argv` representation
- Touching `find_crlf` / `parse_int` / the multibulk loop
- Pre-publish (always re-run the ≥1h fuzz)
