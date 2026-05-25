# kevy-cli

A small `redis-cli`-style client for [kevy](https://crates.io/crates/kevy) — or
any RESP server. Pure Rust, **zero third-party dependencies** (just
[kevy-resp](https://crates.io/crates/kevy-resp) + `std`).

```sh
# one-shot
kevy-cli -p 6379 set foo bar
kevy-cli -p 6379 get foo

# interactive REPL
kevy-cli -h 127.0.0.1 -p 6379
127.0.0.1:6379> hset user:1 name alice
(integer) 1
127.0.0.1:6379> hgetall user:1
1) "name"
2) "alice"
```

- `-h <host>` (default `127.0.0.1`), `-p <port>` (default `6379`).
- Replies are pretty-printed redis-cli-style (errors, integers, bulk, arrays,
  nil); a one-shot RESP error exits non-zero.
- `#![forbid(unsafe_code)]`.

## License

Licensed under either of [MIT](../../LICENSE-MIT) or
[Apache-2.0](../../LICENSE-APACHE) at your option.
