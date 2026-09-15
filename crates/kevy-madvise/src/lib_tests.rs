//! Unit tests for `kevy-madvise` (split out of `lib.rs` for the
//! 500-line file rule; test files are exempt from it).

use super::*;

#[test]
fn no_call_below_two_pages() {
    // Smaller than 2 * 4 KiB: short-circuit, never reaches the syscall.
    // We cannot directly assert "no syscall" without a hook, but the
    // function must at least return cleanly on a tiny buffer.
    let buf = [0u8; 1024];
    advise_hugepage(buf.as_ptr(), buf.len());
}

#[test]
fn unaligned_buffer_does_not_panic() {
    // 16 KiB unaligned buffer; the wrapper rounds inward and either
    // calls madvise on the aligned subset or no-ops. Either way, no
    // panic, no UB.
    let buf = vec![0u8; 16 * 1024];
    advise_hugepage(buf.as_ptr().wrapping_add(7), buf.len() - 7);
}

#[test]
fn zero_length_is_noop() {
    advise_hugepage(core::ptr::null(), 0);
}

#[test]
fn large_aligned_region_runs() {
    // 64 KiB region — enough to clear all the page-alignment guards.
    // On Linux this issues the syscall; on macOS it's compile-time
    // out. We only assert the function completes.
    let buf = vec![0u8; 64 * 1024];
    advise_hugepage(buf.as_ptr(), buf.len());
}

/// The hint must actually be accepted by the kernel, not merely
/// issued.
///
/// Every other test in this module checks that a call returns
/// cleanly, and one of them says so out loud: "We cannot directly
/// assert 'no syscall' without a hook". So they pass whether the
/// kernel honours the advice or refuses every one of them — which is
/// exactly what a hardcoded 4 KiB page size did on a 64 KiB-page
/// kernel, silently, because the return value was discarded.
///
/// `last_advised_bytes` is that hook.
#[cfg(target_os = "linux")]
#[test]
fn a_hugepage_hint_on_a_real_region_is_accepted_by_the_kernel() {
    // Big enough that a whole aligned huge page fits inside it
    // whatever the region's starting address turns out to be.
    let len = HUGE_PAGE * 4;
    let Some(p) = mmap_anon_aligned_2mb(len) else {
        // No mapping available (constrained container): say so rather
        // than pass.
        panic!("could not map a region to advise — this test verified nothing");
    };
    advise_hugepage(p.as_ptr(), len);
    let got = last_advised_bytes();
    assert!(
        got >= HUGE_PAGE * 2,
        "the kernel accepted {got} bytes of a {len} byte region — a refused hint reads \
         exactly like a granted one unless this is checked"
    );
    // SAFETY: unmapping exactly what `mmap_anon_aligned_2mb` returned.
    unsafe { munmap_2mb(p, len) };
}

/// And a region too small to hold an aligned huge page is not
/// advised at all — the syscall costs a write lock and a possible VMA
/// split in exchange for a promotion that cannot happen.
#[cfg(target_os = "linux")]
#[test]
fn a_region_too_small_to_promote_is_not_advised() {
    let buf = vec![0u8; HUGE_PAGE];
    advise_hugepage(buf.as_ptr(), buf.len());
    assert_eq!(last_advised_bytes(), 0, "advised a region that cannot be promoted");
}
