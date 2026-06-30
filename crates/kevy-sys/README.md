# kevy-sys

The network-boundary layer for [kevy](https://crates.io/crates/kevy) —
a tiny, zero-dependency, pure-Rust wrapper over the sockets and readiness
poller the kevy server needs.

This is one of kevy's three OS-boundary crates. The other two are
publishable, general-purpose crates:

- [`kevy-uring`](https://crates.io/crates/kevy-uring) — pure-Rust
  io_uring (Linux completion engine), carved out of this crate.
- [`kevy-madvise`](https://crates.io/crates/kevy-madvise) — pure-Rust
  `madvise(MADV_HUGEPAGE)` hint, also carved out.

`kevy-sys` is **server-internal** — its API surface is hand-curated to the
exact subset of sockets / pollers kevy's reactor uses. A third party
would compare against `libc` / `nix` / `rustix` / `mio` and find it
missing too much; it's not a generic foundation, it's the OS-boundary
piece of the kevy server.

- **Sockets** — `tcp_listen` / `tcp_listen_reuseport` / `unix_listen`
  (`AF_UNIX` stream, unlink-before-bind + `chmod 0777`, mirroring
  valkey/redis), non-blocking I/O, `TCP_NODELAY`, owned fds that
  close on drop.
- **Readiness poller** — one API over **kqueue** (macOS) and **epoll** (Linux).
- **Cross-thread `Waker`** — a self-pipe to wake a blocked poller.
- Cross-platform `sockaddr_in` / `kevent` / `epoll_event` layouts (incl.
  the x86_64 packed `epoll_event`).

```rust,no_run
use kevy_sys::{Poller, tcp_listen};

let listener = tcp_listen([127, 0, 0, 1], 6379, 1024)?;
listener.set_nonblocking()?;
let poller = Poller::new()?;
poller.add(listener.raw(), true, false)?;
# Ok::<(), std::io::Error>(())
```

## Safety

`unsafe` is confined to the `ffi` module and its wrappers; the public API
is safe. See the crate docs' *Safety* section for the ABI invariants.

## License

Licensed under either of [MIT](LICENSE-MIT) or
[Apache-2.0](LICENSE-APACHE) at your option.
