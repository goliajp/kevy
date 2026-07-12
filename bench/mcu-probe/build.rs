//! Hand the linker script to rustc from here, not from `.cargo/config.toml`.
//!
//! A `RUSTFLAGS` environment variable overrides `target.*.rustflags` in the
//! cargo config *wholesale*. With `RUSTFLAGS` set (CI sets `-D warnings`),
//! `-C link-arg=-Tlink.x` was silently dropped, lld fell back to its default
//! layout, and the firmware booted with no vector table at address 0 — a
//! lockup before the first instruction of `main`. `rustc-link-arg` does not
//! travel through `RUSTFLAGS`, so it cannot be knocked out that way.

fn main() {
    let dir = std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR");
    println!("cargo::rustc-link-arg=-T{dir}/link.x");
    println!("cargo::rerun-if-changed=link.x");
}
