# kevy-resp-client

A minimal blocking [RESP2](https://redis.io/docs/latest/develop/reference/protocol-spec/)
client over TCP — pure Rust, zero dependencies.

Pairs with [`kevy-resp`](https://crates.io/crates/kevy-resp): this crate adds
the TCP connect + send-request / read-reply loop on top. Suitable for tests,
admin tools, and any caller that wants a stripped-down synchronous client
without dragging in async or extra deps.

```rust,no_run
use kevy_resp::Reply;
use kevy_resp_client::RespClient;

let mut c = RespClient::connect("127.0.0.1", 6379).unwrap();
let reply = c.request(&[b"PING".to_vec()]).unwrap();
assert!(matches!(reply, Reply::Simple(b) if b == b"PONG"));
```

Speaks any Redis-compatible server (kevy, Redis 7.x, Valkey).

## Scope

Intentionally tiny: connect, request/reply, no pipelining helpers, no
pub/sub state machine. For pipelining, queue several commands and call
`request` after each — every reply lands inline.

## License

MIT OR Apache-2.0
