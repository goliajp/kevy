//! Signal constants + the `signal(2)` handler installer.

use crate::ffi;
use core::ffi::c_int;

/// `SIGTERM` constant.
pub const SIGTERM: c_int = 15;
/// `SIGINT` constant (Ctrl-C).
pub const SIGINT: c_int = 2;
/// `SIGXFSZ` constant (write would exceed `RLIMIT_FSIZE`).
/// Default action is `Core` — installing a handler prevents the
/// kernel from dumping core and lets kevy exit gracefully on
/// disk-full / fsize-limit conditions.
pub const SIGXFSZ: c_int = 25;

/// Install a C-style handler for `signum`, a wrapper around `signal(2)`.
///
/// Typical use: the handler stores into a `static AtomicBool` which the
/// main loop polls.
///
/// # The handler must be async-signal-safe
///
/// It runs in signal context — on an arbitrary thread, between two
/// arbitrary instructions, possibly while that same thread already holds
/// the allocator's lock. It may touch atomics and `write(2)` to a
/// self-pipe, and essentially nothing else: no allocation, no locks, no
/// non-reentrant libc. A handler that allocates can self-deadlock or
/// corrupt the heap.
///
/// **This obligation is on the caller and this function is safe, which
/// is a mismatch.** A safe function that admits undefined behaviour when
/// its (unenforceable) precondition is broken should be `unsafe`, and
/// making it so is a breaking change this crate has not taken yet — see
/// `.claude/OPEN-QUESTIONS-6.4.md`. Every caller in this workspace is an
/// atomic store or a no-op, so nothing is wrong today; what is missing
/// is anything making that a requirement rather than a coincidence.
///
/// The return value of `signal(2)` is discarded, so a refusal —
/// `SIG_ERR`, which is what installing on `SIGKILL` or `SIGSTOP` gives
/// — is silent. Reporting it needs a return type, which is the same
/// breaking change.
pub fn install_signal_handler(signum: c_int, handler: extern "C" fn(c_int)) {
    // SAFETY: `signum` is an int and `handler` is a live `extern "C"` fn
    // pointer with the signature `signal(2)` expects, so the call itself
    // reads no Rust memory and cannot alias. What it does NOT establish
    // is that `handler` is async-signal-safe when the kernel later runs
    // it — that premise cannot be checked here and is stated above as a
    // caller obligation.
    //
    // The previous note here read "signal(2) is signal-safe; we just
    // register a static handler. No allocation, no aliasing." That is
    // true of the registration and says nothing about the only thing
    // that can go wrong.
    unsafe {
        ffi::signal(signum, handler);
    }
}
