# kevy-madvise

Pure-Rust [`madvise(2)`](https://man7.org/linux/man-pages/man2/madvise.2.html)
hints — currently a single entry point for `MADV_HUGEPAGE`. Hand-bound
against glibc with `unsafe extern "C"`. **No `libc` crate, no third-party
dependency.** Off Linux every entry point compile-time no-ops, so the
crate is `cargo build`-clean on every target without `cfg` gates at the
call site.

```rust
use kevy_madvise::advise_hugepage;

let buf = vec![0u8; 64 * 1024 * 1024]; // 64 MB metadata array
advise_hugepage(buf.as_ptr(), buf.len());
// kernel's khugepaged is now free to promote 2 MB regions in place
```

## Why a separate crate

Carved out of `kevy-sys` so it can be used by other library crates (like
[`kevy-map`](https://crates.io/crates/kevy-map)) without dragging in the
rest of kevy's OS-boundary internals (sockets, pollers, …). The wrapper has
no dependencies on the rest of kevy and is generic enough to be useful
anywhere a Rust process wants a tiny THP hint without the `libc` crate.

Part of the [kevy](https://crates.io/crates/kevy) key–value server.

## Safety

`unsafe` is confined to one `extern "C"` declaration of `madvise` and one
wrapper call site. The wrapper rounds the request to page boundaries,
never reads or writes Rust memory, and silently no-ops on `EINVAL` —
making the public API safe.

## License

Licensed under either of [MIT](LICENSE-MIT) or
[Apache-2.0](LICENSE-APACHE) at your option.
