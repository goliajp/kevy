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

## SQL schema compiler

```sh
kevy-cli sql compile schema.sql                           # print the compiled script
kevy-cli sql compile schema.sql --apply --url 127.0.0.1:6004
```

The [`kevy-sql`](https://crates.io/crates/kevy-sql) declaration-time
compiler: `CREATE TABLE` / `CREATE [UNIQUE] INDEX` /
single-table `CREATE VIEW` compile ONCE into explicit `TABLE.DECLARE`
and `VIEW.CREATE` commands plus `IDX.QUERY` query cards (`$N`
templates the app fills at runtime). `--apply` runs the declaration
commands against a server, printing each reply, and exits non-zero on
any error reply. This is a build step, like a migration tool — ad-hoc
runtime SQL stays refused by the engine (Law 3). The full walk-through
lives in the cookbook's "Porting a PG/MySQL schema" chapter.

## License

Licensed under either of [MIT](../../LICENSE-MIT) or
[Apache-2.0](../../LICENSE-APACHE), at your option.
