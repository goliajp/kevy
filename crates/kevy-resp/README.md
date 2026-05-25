# kevy-resp

A zero-dependency [RESP] (REdis Serialization Protocol) codec in pure Rust.

Part of [kevy](https://crates.io/crates/kevy), a single-machine, Redis-compatible
key–value server — but usable standalone for anything that speaks RESP.

- **Incremental parsing** — [`parse_command`] returns `Ok(None)` when the buffer
  holds only a partial frame, so it drops straight into a streaming read loop.
- **Both request forms** — RESP2 multi-bulk (`*N\r\n$len\r\n…`) and inline
  (`PING\r\n`).
- **Reply encoders** that append to a caller-owned `Vec<u8>` (no per-call
  allocation): simple strings, errors, integers, bulk/null-bulk, array headers.
- **Zero dependencies**, `#![forbid(unsafe_code)]`-friendly (no `unsafe`).

```rust
use kevy_resp::{encode_simple_string, parse_command};

let (cmd, consumed) = parse_command(b"*1\r\n$4\r\nPING\r\n").unwrap().unwrap();
assert_eq!(cmd, vec![b"PING".to_vec()]);
assert_eq!(consumed, 14);

let mut out = Vec::new();
encode_simple_string(&mut out, "PONG");
assert_eq!(out, b"+PONG\r\n");
```

## License

Licensed under either of [MIT](../../LICENSE-MIT) or
[Apache-2.0](../../LICENSE-APACHE) at your option.

[RESP]: https://redis.io/docs/latest/develop/reference/protocol-spec/
[`parse_command`]: https://docs.rs/kevy-resp/latest/kevy_resp/fn.parse_command.html
