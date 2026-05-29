# kevy-uring

Pure-Rust [`io_uring`](https://kernel.dk/io_uring.pdf) bindings against
the Linux kernel ABI. A completion-based engine: submit reads / writes /
accepts into a shared submission queue (SQ), then reap their results from
the completion queue (CQ) — batching many operations into one
`io_uring_enter` syscall.

- `io_uring_setup` / `io_uring_enter` / `io_uring_register` issued as raw
  syscalls. **No `liburing` C dependency, no `libc` crate, no
  third-party Rust dependency.**
- SQ / CQ / SQE regions `mmap`'d and driven through the documented
  head/tail cursors, with the appropriate Acquire / Release fences against
  the kernel-side updates.
- Multishot `recv` + provided-buffer ring (kernel 5.19+) supported, the
  same primitives a thread-per-core reactor needs.
- **Linux-only**: on every other target the crate compiles to an empty
  module that any caller can `cfg`-gate.

```rust,ignore
use std::io::Write;
use std::net::TcpListener;
use std::os::fd::AsRawFd;
use kevy_uring::IoUring;

let listener = TcpListener::bind("127.0.0.1:0")?;
let mut ring = IoUring::new(64)?;
assert!(ring.prep_accept(listener.as_raw_fd(), /* user_data */ 1));
ring.submit_and_wait(1)?;
ring.for_each_completion(|c| println!("accepted fd {}", c.res));
# Ok::<(), std::io::Error>(())
```

## Why a separate crate

Carved out of [`kevy-sys`](https://crates.io/crates/kevy-sys) so the
engine can be reused independently of kevy's network internals (sockets,
readiness pollers). The bindings are generic Linux infrastructure;
nothing here is specific to kevy's command surface.

Part of the [kevy](https://crates.io/crates/kevy) key–value server.

## Safety

The shared ring cursors are accessed as `AtomicU32` over the `mmap`'d
memory (the kernel is the other party): the producer publishes the SQ
tail with `Release` and reads the SQ head with `Acquire`; the consumer
reads the CQ tail with `Acquire` and publishes the CQ head with
`Release`. `IoUring` owns its ring fd and three mappings, freed on drop.

`unsafe` is confined to the FFI module's `extern "C"` declarations and
the wrappers that read / write through the mmap'd ring memory. Every
`unsafe` block carries a `SAFETY:` comment naming the kernel invariant
it relies on.

## License

Licensed under either of [MIT](LICENSE-MIT) or
[Apache-2.0](LICENSE-APACHE) at your option.
