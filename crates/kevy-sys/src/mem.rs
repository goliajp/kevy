//! Memory-bound auto-detection: the OS-boundary probe behind the
//! tiering budget's `auto` / percent forms.
//!
//! - **Linux**: the cgroup v2 limit (`/sys/fs/cgroup/memory.max` — a
//!   byte count, or the literal `max` = unlimited) combined with
//!   `/proc/meminfo`'s `MemAvailable:` line (kB). When both resolve the
//!   bound is their **min** (a container limit below the host's
//!   available memory must win, and vice versa).
//! - **macOS**: `sysctlbyname("hw.memsize")` through the hand-written
//!   binding in [`crate::ffi`] (0-dep charter — no `libc` crate).
//!
//! The parsers take explicit paths so unit tests drive them with
//! fixture files; [`detected_memory_bound`] wires the real ones.

#[cfg(any(target_os = "linux", target_os = "android"))]
use std::path::Path;

/// The detected memory bound in bytes, or `None` when no probe
/// resolves (exotic platform, masked /proc, cgroup v1-only host with
/// no readable meminfo). Callers treat `None` as "auto/percent budgets
/// cannot resolve" — a named refusal, never a silent guess.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub fn detected_memory_bound() -> Option<u64> {
    let cg = cgroup_v2_limit(Path::new("/sys/fs/cgroup/memory.max"));
    let avail = meminfo_available(Path::new("/proc/meminfo"));
    match (cg, avail) {
        (Some(c), Some(a)) => Some(c.min(a)),
        (one, other) => one.or(other),
    }
}

/// The detected memory bound in bytes (`hw.memsize`), or `None` when
/// the sysctl fails.
#[cfg(any(target_os = "macos", target_os = "ios"))]
pub fn detected_memory_bound() -> Option<u64> {
    let mut val: u64 = 0;
    let mut len = core::mem::size_of::<u64>();
    // SAFETY: `hw.memsize` answers a u64; `len` names the buffer size
    // in/out per sysctlbyname(3). The name is a NUL-terminated literal.
    let rc = unsafe {
        crate::ffi::sysctlbyname(
            c"hw.memsize".as_ptr(),
            (&raw mut val).cast(),
            &raw mut len,
            core::ptr::null_mut(),
            0,
        )
    };
    (rc == 0 && len == core::mem::size_of::<u64>() && val > 0).then_some(val)
}

/// No probe on other targets.
#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios"
)))]
pub fn detected_memory_bound() -> Option<u64> {
    None
}

/// This process's resident set size in bytes, or 0 where no probe
/// exists (size containers from RSS;
/// `used_memory` is the store, not the process).
///
/// - **Linux**: `/proc/self/status` `VmRSS:` (kB — page-size-free,
///   unlike statm's page counts).
/// - **macOS**: `task_info(MACH_TASK_BASIC_INFO).resident_size`
///   through the hand-written binding in [`crate::ffi`].
#[cfg(any(target_os = "linux", target_os = "android"))]
pub fn process_rss_bytes() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| vmrss_bytes(&s))
        .unwrap_or(0)
}

/// Parse the `VmRSS:` line (kB) out of a `/proc/<pid>/status` body.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn vmrss_bytes(status: &str) -> Option<u64> {
    let line = status.lines().find(|l| l.starts_with("VmRSS:"))?;
    let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kb * 1024)
}

/// See the Linux twin above.
#[cfg(any(target_os = "macos", target_os = "ios"))]
pub fn process_rss_bytes() -> u64 {
    // struct mach_task_basic_info: virtual_size u64, resident_size
    // u64, resident_size_max u64, user_time 8B, system_time 8B,
    // policy i32, suspend_count i32 — 48 bytes, natural_t count 12.
    const MACH_TASK_BASIC_INFO: u32 = 20;
    let mut info = [0u64; 6];
    let mut count: u32 = 12;
    // SAFETY: the buffer is 48 bytes as `count` declares; task_info
    // fills at most `count` natural_t words per task_info(2).
    let rc = unsafe {
        crate::ffi::task_info(
            crate::ffi::mach_task_self_,
            MACH_TASK_BASIC_INFO,
            info.as_mut_ptr().cast(),
            &raw mut count,
        )
    };
    if rc == 0 { info[1] } else { 0 }
}

/// No probe on other targets — 0, stated rather than guessed.
#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios"
)))]
pub fn process_rss_bytes() -> u64 {
    0
}

/// Parse a cgroup v2 `memory.max` file: a decimal byte count, or the
/// literal `max` (= no limit → `None`). A missing / unreadable file is
/// also `None` (not in a cgroup, or v1-only host).
#[cfg(any(target_os = "linux", target_os = "android"))]
fn cgroup_v2_limit(path: &Path) -> Option<u64> {
    let text = std::fs::read_to_string(path).ok()?;
    let text = text.trim();
    if text == "max" {
        return None;
    }
    text.parse::<u64>().ok()
}

/// Parse `/proc/meminfo`'s `MemAvailable:` line (value in kB → bytes).
/// Absent line / unreadable file = `None`.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn meminfo_available(path: &Path) -> Option<u64> {
    let text = std::fs::read_to_string(path).ok()?;
    for line in text.lines() {
        let Some(rest) = line.strip_prefix("MemAvailable:") else {
            continue;
        };
        let kb: u64 = rest.trim().trim_end_matches("kB").trim().parse().ok()?;
        return Some(kb.saturating_mul(1024));
    }
    None
}

#[cfg(test)]
mod tests {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    mod linux {
        use super::super::{cgroup_v2_limit, meminfo_available};
        use std::path::PathBuf;

        /// A throwaway fixture file (unique per test; removed on drop).
        struct Fixture(PathBuf);
        impl Fixture {
            fn write(name: &str, content: &str) -> Self {
                let p = std::env::temp_dir().join(format!(
                    "kevy-sys-mem-{name}-{}",
                    std::process::id()
                ));
                std::fs::write(&p, content).expect("fixture write");
                Fixture(p)
            }
        }
        impl Drop for Fixture {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(&self.0);
            }
        }

        #[test]
        fn cgroup_value_parses() {
            let f = Fixture::write("cg-val", "2147483648\n");
            assert_eq!(cgroup_v2_limit(&f.0), Some(2_147_483_648));
        }

        #[test]
        fn cgroup_max_means_unlimited() {
            let f = Fixture::write("cg-max", "max\n");
            assert_eq!(cgroup_v2_limit(&f.0), None);
        }

        #[test]
        fn cgroup_missing_file_is_none() {
            let p = std::env::temp_dir().join("kevy-sys-mem-definitely-absent");
            assert_eq!(cgroup_v2_limit(&p), None);
        }

        #[test]
        fn meminfo_memavailable_parses_kb_to_bytes() {
            let f = Fixture::write(
                "meminfo",
                "MemTotal:       16384000 kB\nMemFree:         1024000 kB\n\
                 MemAvailable:    8192000 kB\nBuffers:          123456 kB\n",
            );
            assert_eq!(meminfo_available(&f.0), Some(8_192_000 * 1024));
        }

        #[test]
        fn meminfo_without_memavailable_is_none() {
            let f = Fixture::write("meminfo-absent", "MemTotal: 16384000 kB\n");
            assert_eq!(meminfo_available(&f.0), None);
        }

        #[test]
        fn vmrss_parses_kb_to_bytes() {
            let body = "VmPeak:\t 1000 kB\nVmRSS:\t  512 kB\nVmData:\t 100 kB\n";
            assert_eq!(super::super::vmrss_bytes(body), Some(512 * 1024));
            assert_eq!(super::super::vmrss_bytes("VmPeak: 1 kB\n"), None);
        }
    }

    /// The real sysctl on the dev host: `hw.memsize` must answer a
    /// positive byte count.
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    #[test]
    fn real_sysctl_reports_positive_memsize() {
        let bound = super::detected_memory_bound();
        assert!(bound.is_some_and(|b| b > 0), "hw.memsize probe failed: {bound:?}");
    }

    /// The real RSS probe on any host with one: a running test
    /// process is resident.
    #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos", target_os = "ios"))]
    #[test]
    fn real_process_rss_is_positive() {
        let rss = super::process_rss_bytes();
        assert!(rss > 0, "rss probe answered {rss}");
    }
}
