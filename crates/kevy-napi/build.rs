//! Tell macOS's linker that the `napi_*` symbols are not missing.
//!
//! They live in the node executable and resolve when the addon is
//! loaded. A Linux linker leaves undefined symbols in a shared object
//! alone; ld64 refuses the link unless told to expect them.

fn main() {
    // The napi_* symbols live in the node executable and resolve at load
    // time. Linux linkers leave undefined symbols in a shared object alone;
    // macOS must be told to.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-link-arg-cdylib=-undefined");
        println!("cargo:rustc-link-arg-cdylib=dynamic_lookup");
    }
}
