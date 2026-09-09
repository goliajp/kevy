//! The OS boundary: anonymous mapping, unmapping, and returning pages.
//!
//! Three hand-declared `extern "C"` symbols, no `libc` crate — the house
//! rule for OS boundaries. Linux and macOS only; elsewhere every entry
//! point reports failure and the allocator is simply unavailable.
//!
//! # Why not `kevy-madvise`
//!
//! That crate already binds `mmap`/`munmap`/`madvise`, so reusing it was
//! the first choice. It does not fit: it is Linux-only by construction
//! and its contract *is* huge-page advice — every mapping it hands out
//! has `MADV_HUGEPAGE` applied. An allocator needs mappings on macOS too
//! (that is where this is developed), and it must be able to *return*
//! pages, which is the property the whole experiment rests on. Widening
//! a crate whose name is its contract costs more than three extern
//! declarations, so the boundary lives here — which is also why
//! `kevy-alloc` is in the recorded unsafe set (allocgate M8).

#[cfg(any(target_os = "linux", target_os = "macos"))]
use core::ffi::c_void;
use core::ptr::NonNull;

#[cfg(any(target_os = "linux", target_os = "macos"))]
unsafe extern "C" {
    fn mmap(
        addr: *mut c_void,
        length: usize,
        prot: i32,
        flags: i32,
        fd: i32,
        offset: i64,
    ) -> *mut c_void;
    fn munmap(addr: *mut c_void, length: usize) -> i32;
    fn madvise(addr: *mut c_void, length: usize, advice: i32) -> i32;
    fn sysconf(name: i32) -> i64;
}

/// `_SC_PAGESIZE`. Linux says 30, macOS says 29 — the one constant in
/// this file that is not shared, which is itself why it is worth asking
/// the system rather than assuming.
#[cfg(target_os = "linux")]
const SC_PAGESIZE: i32 = 30;
#[cfg(target_os = "macos")]
const SC_PAGESIZE: i32 = 29;

#[cfg(any(target_os = "linux", target_os = "macos"))]
const PROT_READ: i32 = 0x1;
#[cfg(any(target_os = "linux", target_os = "macos"))]
const PROT_WRITE: i32 = 0x2;
#[cfg(any(target_os = "linux", target_os = "macos"))]
const MAP_PRIVATE: i32 = 0x2;

#[cfg(target_os = "linux")]
const MAP_ANONYMOUS: i32 = 0x20;
#[cfg(target_os = "macos")]
const MAP_ANONYMOUS: i32 = 0x1000;

/// Discard the contents of a resident range and return the physical
/// pages to the OS, keeping the mapping addressable.
///
/// Linux `MADV_DONTNEED` (4) drops the pages outright: RSS falls and a
/// later touch faults in a zero page. macOS has no equivalent that
/// *guarantees* the drop — `MADV_FREE` (5) marks pages reclaimable and
/// the kernel takes them under pressure, so RSS may not move promptly.
/// The difference is why M4 (reclaim proven directly) is asserted on
/// Linux and reported as informational on macOS rather than being
/// quietly assumed to hold on both.
#[cfg(target_os = "linux")]
const MADV_DISCARD: i32 = 4;
#[cfg(target_os = "macos")]
const MADV_DISCARD: i32 = 5;

/// The system page size this module assumes for rounding.
///
/// It is a constant because the geometry is: `PAGES_PER_SPAN` is
/// `SPAN_BYTES / PAGE`, and `SpanMeta::discarded` is exactly a `u16` for
/// the sixteen pages that gives. What is NOT safe is assuming the
/// system agrees — see [`page_size_matches`].
pub const PAGE: usize = 4096;

/// Whether the running system's page size is the one the geometry above
/// was built for.
///
/// This is not pedantry. The development machine for this crate reports
/// 16384 and the bench box reports 4096, and the reclaim path issues
/// `madvise` on ranges computed at 4096 granularity. On a 16 KiB-page
/// system those ranges are not page-aligned; macOS answers 0 anyway and
/// reclaims nothing, and the three callers in `reclaim.rs` discard the
/// return value — so the allocator marks the pages discarded, counts
/// them in `returned`, lowers `predicted_resident()`, and the kernel
/// hands back nothing at all.
///
/// A measuring device that fails in the shape of data. The README's
/// headline "plus returning the pages = 29.3 ns/op" was taken on the
/// 16384 machine, which means it timed a run of `madvise` calls that
/// could not do what the line says they did.
///
/// Answered once and cached: `sysconf` is a call, and this sits under
/// the reclaim tick.
///
/// ```
/// // Stable across calls — it is answered once and cached, and an
/// // answer that flapped would be worse than either value.
/// let a = kevy_alloc::os::page_size_matches();
/// assert_eq!(a, kevy_alloc::os::page_size_matches());
/// ```
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn page_size_matches() -> bool {
    use core::sync::atomic::{AtomicU8, Ordering};
    static ANSWER: AtomicU8 = AtomicU8::new(0); // 0 unknown, 1 yes, 2 no
    if let Some(known) = decode_memo(ANSWER.load(Ordering::Relaxed)) {
        return known;
    }
    // SAFETY: `sysconf` reads no Rust memory and takes an int.
    let got = unsafe { sysconf(SC_PAGESIZE) };
    let ok = usable_page_size(got);
    ANSWER.store(encode_memo(ok), Ordering::Relaxed);
    ok
}

/// The memo's three states, as a function of the stored byte.
///
/// Every machine takes exactly one of `1` and `2` forever, so whichever
/// it is not is unreachable code there — a permanently dead region on
/// any single platform's coverage run. Splitting the decode out makes
/// all three answerable from a test anywhere.
#[cfg(any(target_os = "linux", target_os = "macos"))]
const fn decode_memo(stored: u8) -> Option<bool> {
    match stored {
        1 => Some(true),
        2 => Some(false),
        _ => None,
    }
}

/// The inverse of [`decode_memo`], kept beside it so the two cannot
/// drift into disagreeing about which byte means what.
#[cfg(any(target_os = "linux", target_os = "macos"))]
const fn encode_memo(answer: bool) -> u8 {
    if answer { 1 } else { 2 }
}

/// The decision, separated from the syscall that supplies it.
///
/// Every machine gives one answer, so the other branch cannot be
/// executed where it runs — which is how a coverage ratchet ends up
/// holding a permanently dead region, and how the interesting half (a
/// mismatch: the one that makes reclaim inert) stays untested on
/// exactly the machines where it is false. Taking the measurement as an
/// argument makes both answers reachable from a test anywhere.
#[must_use]
pub(crate) fn usable_page_size(measured: i64) -> bool {
    measured > 0 && measured as u64 == PAGE as u64
}

/// Non-Unix has no reclaim path at all, so nothing can be misreported.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn page_size_matches() -> bool {
    false
}

/// Round `n` up to a multiple of `align`, which must be a power of two.
#[must_use]
pub const fn round_up(n: usize, align: usize) -> usize {
    (n + align - 1) & !(align - 1)
}

/// Map `len` bytes anonymously with the returned address aligned to
/// `align` bytes.
///
/// `align` must be a power of two and a multiple of [`PAGE`]; `len` must
/// be a non-zero multiple of `align`. Over-allocates by one alignment
/// unit and trims both sides, because `mmap` only promises page
/// alignment. Returns `None` on failure — never panics, because an
/// allocator that panics on OOM is worse than one that reports it.
pub fn map_aligned(len: usize, align: usize) -> Option<NonNull<u8>> {
    if len == 0 || !align.is_power_of_two() || !len.is_multiple_of(align) {
        return None;
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        None
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        if cfg!(miri) {
            return None;
        }
        let total = len.checked_add(align)?;
        // SAFETY: the canonical anonymous mapping call. No Rust memory is
        // read or written; a null hint lets the kernel choose the address.
        let raw = unsafe {
            mmap(
                core::ptr::null_mut(),
                total,
                PROT_READ | PROT_WRITE,
                MAP_PRIVATE | MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if raw as isize == -1 {
            return None;
        }
        NonNull::new(trim(raw as usize, total, len, align) as *mut u8)
    }
}

/// Trim an over-allocated mapping down to `len` bytes starting at the
/// first `align`-aligned address inside it, unmapping both offcuts.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn trim(raw: usize, total: usize, len: usize, align: usize) -> usize {
    let start = (raw + align - 1) & !(align - 1);
    let prefix = start - raw;
    let suffix = total - prefix - len;
    if prefix > 0 {
        // SAFETY: `prefix` bytes at `raw` are part of the mapping we
        // just made and are not otherwise referenced.
        unsafe { munmap(raw as *mut c_void, prefix) };
    }
    if suffix > 0 {
        // SAFETY: same mapping, the tail past the aligned region.
        unsafe { munmap((start + len) as *mut c_void, suffix) };
    }
    start
}

/// Unmap `len` bytes at `ptr`.
///
/// # Safety
/// `ptr`/`len` must describe a live mapping produced by [`map_aligned`]
/// (or a whole sub-range of one that is no longer referenced).
pub unsafe fn unmap(ptr: NonNull<u8>, len: usize) {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        if cfg!(miri) {
            return;
        }
        // SAFETY: delegated to the caller's contract.
        unsafe { munmap(ptr.as_ptr().cast(), len) };
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (ptr, len);
    }
}

/// Return the physical pages backing `len` bytes at `ptr` to the OS
/// while keeping the range mapped and addressable.
///
/// The range must be page-aligned and a whole number of pages. Contents
/// are discarded: a later read sees zeroes, which is why only spans with
/// no live slots are ever passed here.
///
/// # Safety
/// `ptr`/`len` must lie inside a live mapping from [`map_aligned`], and
/// no live data may remain in the range.
pub unsafe fn discard(ptr: NonNull<u8>, len: usize) -> bool {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        if cfg!(miri) {
            return false;
        }
        // SAFETY: delegated to the caller's contract; madvise reads no
        // Rust memory.
        unsafe { madvise(ptr.as_ptr().cast(), len, MADV_DISCARD) == 0 }
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (ptr, len);
        false
    }
}

/// Whether this target can map memory at all. Used by tests and by the
/// heap's construction path to fail fast rather than mysteriously.
#[must_use]
pub const fn available() -> bool {
    cfg!(any(target_os = "linux", target_os = "macos")) && !cfg!(miri)
}

#[cfg(test)]
mod page_size_tests {
    use super::{PAGE, usable_page_size};

    /// The memo round-trips, and its unknown state is distinct from
    /// both answers. Whichever of the two a machine stores, the other
    /// arm is unreachable there — which is why this is tested through a
    /// pure function rather than left to a coverage run that can only
    /// ever see one of them.
    #[test]
    fn the_memo_round_trips_and_unknown_is_neither_answer() {
        use super::{decode_memo, encode_memo};
        assert_eq!(decode_memo(encode_memo(true)), Some(true));
        assert_eq!(decode_memo(encode_memo(false)), Some(false));
        assert_eq!(decode_memo(0), None, "0 is unasked, not an answer");
        assert_ne!(encode_memo(true), 0, "an answer must not read as unasked");
        assert_ne!(encode_memo(false), 0);
    }

    /// Both answers, including the one this machine cannot give. The
    /// 16384 case is not hypothetical — it is every Apple Silicon Mac,
    /// and it is the case in which page-granular reclaim does nothing.
    #[test]
    fn only_an_exact_match_is_usable() {
        assert!(usable_page_size(PAGE as i64));
        assert!(!usable_page_size(16384), "a 16 KiB page is not our 4 KiB arithmetic");
        assert!(!usable_page_size(1024), "a smaller page misaligns the same way");
        // `sysconf` reports failure as -1, and a negative cast to
        // unsigned is how a refusal becomes an enormous page size.
        assert!(!usable_page_size(-1), "a failed sysconf must not read as a match");
        assert!(!usable_page_size(0));
    }
}
