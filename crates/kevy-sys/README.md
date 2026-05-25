# kevy-sys

The single OS-boundary layer for [kevy](https://crates.io/crates/kevy) — a tiny,
zero-dependency, pure-Rust wrapper over the sockets and readiness poller a
network server needs.

This is the **only** place kevy touches libc, and only for primitives the kernel
can't expose otherwise. Everything is hand-bound with `unsafe extern "C"` — **no
`libc` crate, no third-party dependencies.** The symbols resolve through glibc
(Linux) / libSystem (macOS), both already linked by `std`.

- **Sockets** — `tcp_listen` / `tcp_listen_reuseport`, non-blocking I/O,
  `TCP_NODELAY`, owned fds that close on drop.
- **Readiness poller** — one API over **kqueue** (macOS) and **epoll** (Linux).
- **Cross-thread `Waker`** — a self-pipe to wake a blocked poller.
- Cross-platform `sockaddr_in` / `kevent` / `epoll_event` layouts (incl. the
  x86_64 packed `epoll_event`).

```rust,no_run
use kevy_sys::{Poller, tcp_listen};

let listener = tcp_listen([127, 0, 0, 1], 6379, 1024)?;
listener.set_nonblocking()?;
let poller = Poller::new()?;
poller.add(listener.raw(), true, false)?;
# Ok::<(), std::io::Error>(())
```

## Safety

`unsafe` is confined to the `ffi` module and its wrappers; the public API is
safe. See the crate docs' *Safety* section for the ABI invariants.

## License

Licensed under either of [MIT](../../LICENSE-MIT) or
[Apache-2.0](../../LICENSE-APACHE) at your option.
