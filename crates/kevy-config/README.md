# kevy-config

The TOML config schema for the kevy server, plus a small TOML subset
parser tailored to it.

- Zero `crates.io` dependencies.
- `#![forbid(unsafe_code)]`.
- Builds for Linux (`x86_64`, `aarch64`), macOS, and
  `wasm32-unknown-unknown`.

## Install

```sh
cargo add kevy-config
```

## Quick start

```rust
use kevy_config::{CliOverrides, Config};

let mut cfg = Config::load(None).expect("config error");
cfg.merge_env(std::env::vars()).expect("bad env value");
let mut cli = CliOverrides::default();
cli.bind = Some([0, 0, 0, 0]);
cfg.merge_cli(cli).expect("bad cli value");

println!("listening on {:?}:{}", cfg.server.bind, cfg.server.port);
```

## Precedence chain

Top wins:

1. CLI overrides via `Config::merge_cli`.
2. Environment variables via `Config::merge_env`
   (`KEVY_BIND`, `KEVY_PORT`, `KEVY_THREADS`, `KEVY_DIR`, `KEVY_AOF`).
3. TOML file via `Config::load`. An explicit path is honoured; with
   `None` the loader auto-detects `$KEVY_DIR/kevy.toml`, then
   `./kevy.toml`, then `/etc/kevy/kevy.toml`.
4. `Config::default()`.

## Schema sections

| Section | Keys |
|---|---|
| `[server]` | `bind` · `port` · `threads` · `accept_shards` · `packed_rows` · `max_clients` · `data_dir` |
| `[persistence]` | `aof` · `appendfsync` · `auto_aof_rewrite_percentage` · `auto_aof_rewrite_min_size` · `auto_aof_rewrite_bytes` · `auto_aof_rewrite_interval_secs` · `replay_resync` |
| `[memory]` | `maxmemory` · `maxmemory_policy` |
| `[expiry]` | `hz` · `sample` |
| `[log]` | `level` · `output` |
| `[notification]` | `notify_keyspace_events` |
| `[advanced]` | `spin_limit` · `park_timeout_ms` · `tick_check_every` · `ring_capacity` |
| `[slowlog]` | `slower_than_micros` · `max_len` |
| `[cluster]` | `enabled` · `port_base` · `node_id` · `elect_port_base` · `announce_ip` · `announce_port_base` · `secure` · `peer_keys` · `peers` · `scopes` |
| `[replication]` | `role` · `upstream` · `listen_port_base` · `replication_buffer_size` · `reconnect_window_ms` · `single_source` · `min_replicas_to_write` · `replica_max_staleness_ms` · `min_replicas_max_lag_ms` · `replica_read_only` · `secure` · `upstream_key` · `replica_keys` |
| `[feed]` | `enabled` · `feed_buffer_size` |
| `[tiering]` | `budget` · `spill_dir` |
| `[lua]` | `time_limit_ms` · `allow_dialects` |
| `[metrics]` | `listen_port` |
| `[audit]` | `log_path` |
| `[secure]` | `private_key_file` · `listen_port` · `client_keys` · `cluster_port_base` · `announce_cluster_port_base` |

An annotated starter config lives at
[`crates/kevy/kevy.toml.example`](https://github.com/goliajp/kevy/blob/develop/crates/kevy/kevy.toml.example).

## TOML subset

This is not a general-purpose TOML parser. It supports exactly what
kevy's config needs:

- `[section]` table headers (one level deep).
- `key = value` with `value` ∈ `{string, integer, boolean, size
  literal}`.
- String literals: `"double-quoted"` (with escapes) and
  `'single-quoted'` (raw).
- Integers: signed decimal, underscore separators
  (`1_000_000`).
- Booleans: `true` / `false`.
- `# comment` to end of line.

Size literals (`"64mb"`, `"2gb"`, `"512kb"`) are a kevy extension
parsed when a schema field expects bytes. Suffixes are
case-insensitive; multipliers are binary (1 KB = 1024 bytes) to match
the Redis convention.

Not supported: dotted keys, multi-line strings, arrays, arrays of
tables, inline tables, datetime literals.

## Errors

`ConfigError` has three variants:

- `IoOpen` — could not open the file.
- `Parse` — tokenizer or parser error with `(line, col)` plus a
  message.
- `Schema` — value rejected by the schema (unknown enum, out-of-range
  integer, wrong type) with `(line, field, msg)`.

Parser failures are never silently downgraded; startup fails loudly
so misconfigurations surface immediately.

## Why the workspace ships its own TOML parser

The kevy charter forbids `crates.io` dependencies in the default
server stack. The full TOML format includes datetime types,
multi-line strings, dotted keys, and arrays of tables — features the
kevy config does not use. A focused subset parser (about 600 LOC
including lexer + parser + tests) is cheaper than pulling in the
[`toml`](https://crates.io/crates/toml) crate (~10 KLOC, two-phase
parse), and lets the parser stay `forbid(unsafe_code)` and miri-clean.

## License

MIT OR Apache-2.0, at your option.
