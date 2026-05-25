# kevy-resp performance budgets

Regression budgets in `tests/perf_gate.rs` (`cargo test -p kevy-resp --test perf_gate`).
Exploration: `cargo run -p kevy-resp --example bench_resp --release`.

## Path taxonomy

Every command crosses this codec twice: `parse_command` on the request, then a
reply encoder on the response. So both are on the per-command hot path.

- **`parse_command`** — once per request. Allocates the owned `argv`
  (`Command = Vec<Vec<u8>>`): one outer `Vec` + one `Vec<u8>` per argument,
  because the thread-per-core runtime forwards the argv to another core's shard
  and therefore needs it owned (a borrow into the read buffer can't cross the
  channel). Cost is dominated by those allocations.
- **reply encoders** (`encode_bulk`/`encode_simple_string`/`encode_integer`/…)
  — once per reply. They append into a caller-owned, reused buffer, so they
  allocate nothing themselves and are near-free.

## Measured (M-series Mac, release, kevy-bench median, 3 runs; loaded host)

| Op | `Vec<Vec<u8>>` | `Argv` (v0.perf-9) | note |
|---|---:|---:|---|
| `parse_command` GET (2 args) | ~60 ns | **~40 ns** | 2 allocs (buf + ends) vs 3 |
| `parse_command` SET (3 args) | ~70 ns | **~50 ns** | 2 allocs vs 4 |
| `parse_command` PING (inline) | ~30 ns | ~36 ns | inline, rare |
| `encode_bulk` | ~5 ns | ~5 ns | append to reused buffer |
| `encode_simple_string` | ~2 ns | ~2 ns | |
| `encode_integer` | ~5 ns | ~5 ns | |

**Finding (measure-first):** encoders are near-optimal — leave them. `parse` was
**allocation-bound** (the N+1 `Vec` allocations of `Vec<Vec<u8>>`). **Resolved in
v0.perf-9** by switching `Command` to `Argv { buf, ends }` — a two-allocation
flat argv (see `rfcs/2026-05-25-single-alloc-argv.md`): ~1.4–1.5× faster parse on
SET/GET. `find_crlf` only scans the short length-prefix lines (bulk payload is
length-skipped), a few ns — not worth vectorising.

## Regression budgets (`tests/perf_gate.rs`, dev profile, generous headroom)

| Path | Budget | Observed (dev, loaded) | Headroom |
|---|---:|---:|---:|
| `parse_command` SET | 10 µs | ~300 ns–1.5 µs | ~7–30× |
| `parse_command` inline | 5 µs | ~150 ns–800 ns | ~6–30× |
| `encode_bulk` | 5 µs | ~20–80 ns | large |

Loose by design — catch order-of-magnitude regressions (an accidental O(n²)
scan, a per-arg double-allocation), not ns drift.

## When to re-measure

- Changing the `Command` representation (the single-allocation-argv change above).
- Touching `find_crlf` / `parse_int` / the multibulk loop.
