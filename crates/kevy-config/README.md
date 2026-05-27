# kevy-config

Zero-dependency TOML subset parser + schema for the kevy server.

`#![forbid(unsafe_code)]` · 0 crates.io deps · pure Rust · works on
`x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`, `*-apple-darwin`,
and `wasm32-unknown-unknown` (via embedded use).

## TOML subset

Supports just what kevy's config file needs — nothing more, nothing less.

- `[section]` table headers (one level deep)
- `key = value` with `value` ∈ `{string, integer, boolean, size literal}`
- String literals: `"double-quoted"` (with escapes) and `'single-quoted'` (raw)
- Integers: signed decimal, underscore separators (`1_000_000`)
- Booleans: `true` / `false`
- `# comment` to end of line

Size literals are a kevy extension: `"64mb"` / `"2gb"` / `"512kb"` parsed
via [`parse_size`] when a schema field expects bytes. Suffix is
case-insensitive; multipliers are **binary** (1 KB = 1024 bytes) to
match Redis convention.

**Intentionally unsupported** (full TOML features kevy doesn't need):
dotted keys, multi-line strings, arrays / arrays of tables, inline
tables, datetime literals.

## Precedence chain (top wins)

1. CLI flags via [`Config::merge_cli`] (e.g. `--bind 0.0.0.0`)
2. Environment via [`Config::merge_env`] (`KEVY_BIND`, `KEVY_PORT`, …)
3. TOML file via [`Config::load`] — explicit path or auto-detect
   (`$KEVY_DIR/kevy.toml`, then `./kevy.toml`, then `/etc/kevy/kevy.toml`)
4. [`Config::default()`]

## Quick example

```rust
use kevy_config::{Config, CliOverrides};

// Load from auto-detect path, or fall back to defaults.
let mut cfg = Config::load(None).expect("config error");

// Overlay env vars (caller-controlled — easy to fixture in tests).
cfg.merge_env(std::env::vars()).expect("bad env value");

// Overlay CLI overrides (parsed by the caller).
cfg.merge_cli(CliOverrides {
    bind: Some([0, 0, 0, 0]),
    ..CliOverrides::default()
}).expect("bad cli value");

// Hand off to kevy::serve(...).
println!("listening on {:?}:{}", cfg.server.bind, cfg.server.port);
```

## Schema reference

See [`Config`] for the full struct shape and per-field defaults. Quick
summary by section:

| Section | Fields |
|---|---|
| `[server]` | `bind` `port` `threads` `data_dir` |
| `[persistence]` | `aof` `appendfsync` `auto_aof_rewrite_percentage` `auto_aof_rewrite_min_size` |
| `[memory]` | `maxmemory` `maxmemory_policy` |
| `[expiry]` | `hz` `sample` |
| `[log]` | `level` `output` |

A fully-annotated sample lives at
[`crates/kevy/kevy.toml.example`](../kevy/kevy.toml.example).

## Errors

[`ConfigError`] has three variants:

- `IoOpen` — couldn't open the file
- `Parse` — tokenizer / parser error with `(line, col)` + message
- `Schema` — value rejected by the schema (unknown enum, out-of-range
  integer, wrong type) with `(line, field, msg)`

Parser failures are **never** silently downgraded; startup must fail
loudly so misconfigurations surface immediately.

## Why custom TOML

kevy's charter forbids crates.io dependencies. Writing a focused TOML
subset parser (~600 LOC including lexer + parser + tests) was cheaper
than introducing the [`toml`](https://crates.io/crates/toml) crate
(~10 K LOC, two-phase parse, datetime types we don't need) and lets the
parser stay `forbid(unsafe_code)` + miri-clean.

## License

MIT OR Apache-2.0, at your option.
