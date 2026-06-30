# kevy-cli

A small `redis-cli`-style client and operator CLI for kevy, or any
RESP server. Pure Rust, zero `crates.io` dependencies beyond
[`kevy-resp`](https://crates.io/crates/kevy-resp).

```sh
cargo install kevy-cli

# one-shot
kevy-cli -p 6379 SET foo bar
kevy-cli -p 6379 GET foo

# interactive REPL
kevy-cli -h 127.0.0.1 -p 6379
127.0.0.1:6379> HSET user:1 name alice
(integer) 1
127.0.0.1:6379> HGETALL user:1
1) "name"
2) "alice"
```

## Install

```sh
cargo install kevy-cli
```

## Flags

| Flag | Meaning | Default |
|---|---|---|
| `-h <host>` | RESP server hostname | `127.0.0.1` |
| `-p <port>` | RESP server port | `6379` |
| `-s <path>` | Unix-domain socket path (replaces host + port) | — |
| `-t <secs>` | Connection timeout in seconds | `5` |

## Reply rendering

Replies are pretty-printed in the standard `redis-cli` style:

- Errors → `(error) <message>`, exit code `1` for one-shot calls.
- Integers → `(integer) <n>`.
- Bulk strings → `"<bytes>"` or `(nil)`.
- Arrays → numbered list, recursively.

## Backup and restore

```sh
kevy-cli backup --to ./snapshot-2026-07-01.kbackup
kevy-cli restore --from ./snapshot-2026-07-01.kbackup --to /var/lib/kevy
```

`backup` runs against a live server. `restore` writes into a fresh
data directory so the server picks the contents up on the next boot.

## License

Licensed under either of [MIT](../../LICENSE-MIT) or
[Apache-2.0](../../LICENSE-APACHE), at your option.
