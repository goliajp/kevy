//! Signal constants + the `signal(2)` handler installer. Split out of
//! `lib.rs` to keep it under the 500-LOC house cap; verbatim from before
//! the move.

use crate::ffi;
use core::ffi::c_int;

/// **v1.39** — `SIGTERM` constant.
pub const SIGTERM: c_int = 15;
/// **v1.39** — `SIGINT` constant (Ctrl-C).
pub const SIGINT: c_int = 2;
/// **v1.58** — `SIGXFSZ` constant (write would exceed `RLIMIT_FSIZE`).
/// Default action is `Core` — installing a handler prevents the
/// kernel from dumping core and lets kevy exit gracefully on
/// disk-full / fsize-limit conditions.
pub const SIGXFSZ: c_int = 25;

/// **v1.39** — install a C-style handler for `signum`. Safe wrapper
/// around `signal(2)`; the handler must be async-signal-safe (no
/// allocator, no syscall beyond a tiny set). Typical use: handler
/// stores into a `static AtomicBool` which the main loop polls.
pub fn install_signal_handler(signum: c_int, handler: extern "C" fn(c_int)) {
    // SAFETY: signal(2) is signal-safe; we just register a static
    // handler. No allocation, no aliasing.
    unsafe {
        ffi::signal(signum, handler);
    }
}
