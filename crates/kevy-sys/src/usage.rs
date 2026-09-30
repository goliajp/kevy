//! What the calling thread has cost so far: CPU time, page faults and
//! context switches, read around a stretch of code to tell time spent
//! computing from time spent faulting or off the CPU.
//!
//! - **Linux**: `clock_gettime(CLOCK_THREAD_CPUTIME_ID)` and
//!   `getrusage(RUSAGE_THREAD)`, both for this thread alone.
//! - **macOS**: the same clock, but `getrusage` has no per-thread form, so
//!   the fault and switch counts are the whole process's.
//!
//! Two system calls a reading: meant for instrumented builds, not for a
//! request path.

use core::ffi::c_int;

/// The calling thread's usage counters at one instant.
///
/// Built by [`thread_usage`]; the fields are for reading.
///
/// ```
/// if let (Some(a), Some(b)) = (kevy_sys::thread_usage(), kevy_sys::thread_usage()) {
///     assert!(b.cpu_ns >= a.cpu_ns);
/// }
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct ThreadUsage {
    /// CPU time this thread has run, in nanoseconds.
    pub cpu_ns: u64,
    /// Page faults served without I/O.
    pub minor_faults: u64,
    /// Page faults that waited for I/O.
    pub major_faults: u64,
    /// Times the thread gave up the CPU to wait.
    pub voluntary_switches: u64,
    /// Times the scheduler took the CPU away from the thread.
    pub involuntary_switches: u64,
}

#[repr(C)]
struct Timespec {
    sec: i64,
    nsec: i64,
}

// `struct rusage`: two `struct timeval`s (16 bytes each on both 64-bit
// targets, macOS padding its 4-byte microseconds) and fourteen longs.
#[repr(C)]
struct Rusage {
    _times: [i64; 4],
    // maxrss, ixrss, idrss, isrss
    _sizes: [i64; 4],
    minflt: i64,
    majflt: i64,
    // nswap, inblock, oublock, msgsnd, msgrcv, nsignals
    _io: [i64; 6],
    nvcsw: i64,
    nivcsw: i64,
}

const _: () = assert!(size_of::<Rusage>() == 144);
const _: () = assert!(size_of::<Timespec>() == 16);

#[cfg(target_os = "linux")]
const CLOCK_THREAD_CPUTIME_ID: c_int = 3;
#[cfg(target_os = "linux")]
const RUSAGE_WHO: c_int = 1; // RUSAGE_THREAD
#[cfg(target_os = "macos")]
const CLOCK_THREAD_CPUTIME_ID: c_int = 16;
#[cfg(target_os = "macos")]
const RUSAGE_WHO: c_int = 0; // RUSAGE_SELF: macOS has no per-thread form

unsafe extern "C" {
    fn clock_gettime(clock: c_int, tp: *mut Timespec) -> c_int;
    fn getrusage(who: c_int, usage: *mut Rusage) -> c_int;
}

/// The calling thread's CPU time, faults and context switches, or `None`
/// if either call fails. On macOS the fault and switch counts are the
/// process's, not the thread's.
///
/// # Examples
///
/// ```
/// let before = kevy_sys::thread_usage().expect("supported here");
/// let v: Vec<u8> = vec![1; 8 << 20];
/// let after = kevy_sys::thread_usage().expect("supported here");
/// assert!(v.iter().map(|&b| b as u64).sum::<u64>() > 0);
/// assert!(after.cpu_ns >= before.cpu_ns);
/// assert!(after.minor_faults >= before.minor_faults);
/// ```
pub fn thread_usage() -> Option<ThreadUsage> {
    let mut ts = Timespec { sec: 0, nsec: 0 };
    // SAFETY: `ts` is a live, writable `struct timespec` (layout asserted
    // above) and the clock id is the one this target defines.
    if unsafe { clock_gettime(CLOCK_THREAD_CPUTIME_ID, &mut ts) } != 0 {
        return None;
    }
    // SAFETY: an all-zero `struct rusage` is a valid value of it: every
    // field is an integer.
    let mut ru: Rusage = unsafe { core::mem::zeroed() };
    // SAFETY: `ru` is a live, writable `struct rusage` of the size the
    // kernel writes (asserted above); `RUSAGE_WHO` is a valid selector.
    if unsafe { getrusage(RUSAGE_WHO, &mut ru) } != 0 {
        return None;
    }
    let n = |v: i64| u64::try_from(v).unwrap_or(0);
    Some(ThreadUsage {
        cpu_ns: n(ts.sec) * 1_000_000_000 + n(ts.nsec),
        minor_faults: n(ru.minflt),
        major_faults: n(ru.majflt),
        voluntary_switches: n(ru.nvcsw),
        involuntary_switches: n(ru.nivcsw),
    })
}
