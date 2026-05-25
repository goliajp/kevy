# kevy-net

A single-threaded, event-driven reactor in pure Rust, built on
[kevy-sys](https://crates.io/crates/kevy-sys)'s kqueue/epoll poller. Part of
[kevy](https://crates.io/crates/kevy).

Connections are non-blocking and multiplexed by the thousand on one thread. Your
application plugs in through the byte-level [`Service`] trait and never touches
readiness, file descriptors, or the poller — so the I/O engine can evolve (e.g.
to io_uring) without changing your code. A thread-per-core runtime just runs one
reactor per core.

- Level-triggered read/write with per-connection input/output buffers and
  backpressure (write-interest is registered only when output is pending).
- Clean shutdown via an `AtomicBool` stop flag.
- `#![forbid(unsafe_code)]`, one dependency (`kevy-sys`).

```rust
use kevy_net::Service;

struct Echo;
impl Service for Echo {
    fn on_data(&mut self, input: &mut Vec<u8>, output: &mut Vec<u8>) -> bool {
        output.append(input);
        true
    }
}
```

Serve it with `Reactor::new(listener)?.run(&mut Echo, &stop)`.

## License

Licensed under either of [MIT](../../LICENSE-MIT) or
[Apache-2.0](../../LICENSE-APACHE) at your option.
