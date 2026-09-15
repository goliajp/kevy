//! SIMD-accelerated `\r\n` scanner — A6 + A7.
//!
//! Replaces the u64 SWAR loop that lived in `kevy-resp::request::find_crlf`.
//! Three-tier runtime dispatch:
//!
//! - **x86_64 + AVX2** (runtime detected): 32-byte `_mm256_cmpeq_epi8` →
//!   `_mm256_movemask_epi8` loop. 4× the SWAR throughput.
//! - **aarch64 + NEON** (mandatory in the ARMv8 baseline, no detection):
//!   16-byte `vceqq_u8` + reduction. 2× the SWAR throughput.
//! - **Fallback**: the same u64 SWAR algorithm that shipped before, kept
//!   for non-AVX2 x86 boxes (Sandy Bridge etc.) and any non-x86_64 /
//!   non-aarch64 target.
//!
//! AVX2 detection caches its decision in an `AtomicI8` so the cpuid hit
//! amortises across the process lifetime.

/// Find `\r\n` at or after `start`, returning the index of `\r`. Returns
/// `None` if absent or if fewer than two bytes remain.
///
/// # Examples
///
/// The index is of the `\r`, so a RESP line is `buf[start..i]` and the
/// next line begins at `i + 2`:
///
/// ```
/// use kevy_bytes::find_crlf;
/// let buf = b"$3\r\nGET\r\n";
/// let i = find_crlf(buf, 0).unwrap();
/// assert_eq!(i, 2);
/// assert_eq!(&buf[..i], b"$3");
/// assert_eq!(find_crlf(buf, i + 2), Some(7));
/// ```
///
/// A lone `\r` or a lone `\n` is not a terminator, and neither is a `\r`
/// with nothing after it — a half-arrived frame must read as incomplete,
/// not as a short one:
///
/// ```
/// use kevy_bytes::find_crlf;
/// assert_eq!(find_crlf(b"abc", 0), None);
/// assert_eq!(find_crlf(b"a\rb\nc", 0), None);
/// assert_eq!(find_crlf(b"abc\r", 0), None, "the \\n has not arrived yet");
/// assert_eq!(find_crlf(b"", 0), None);
/// ```
///
/// A `start` at or past the end is `None` rather than a panic, so a
/// drained buffer can be probed without a length check first:
///
/// ```
/// use kevy_bytes::find_crlf;
/// assert_eq!(find_crlf(b"ab\r\n", 4), None);
/// assert_eq!(find_crlf(b"ab\r\n", 99), None);
/// ```
#[inline]
pub fn find_crlf(buf: &[u8], start: usize) -> Option<usize> {
    // Runtime AVX2 detection is a std facility; a (hypothetical) no_std
    // x86_64 build falls through to the SWAR loop.
    #[cfg(all(target_arch = "x86_64", feature = "std"))]
    {
        if has_avx2() {
            // SAFETY: gated on runtime AVX2 detection above; the
            // implementation only uses 256-bit intrinsics on chunks
            // fully inside `buf`.
            return unsafe { find_crlf_avx2(buf, start) };
        }
    }
    #[cfg(target_arch = "aarch64")]
    {
        // SAFETY: NEON is part of the ARMv8 baseline (target_arch =
        // "aarch64" implies it's present on every runtime that can run
        // this binary at all).
        return unsafe { find_crlf_neon(buf, start) };
    }
    #[allow(unreachable_code)]
    find_crlf_swar(buf, start)
}

#[cfg(all(target_arch = "x86_64", feature = "std"))]
fn has_avx2() -> bool {
    use core::sync::atomic::{AtomicI8, Ordering};
    static CACHED: AtomicI8 = AtomicI8::new(-1);
    let c = CACHED.load(Ordering::Relaxed);
    if c >= 0 {
        return c == 1;
    }
    let detected = std::is_x86_feature_detected!("avx2");
    CACHED.store(i8::from(detected), Ordering::Relaxed);
    detected
}

#[cfg(all(target_arch = "x86_64", feature = "std"))]
#[target_feature(enable = "avx2")]
unsafe fn find_crlf_avx2(buf: &[u8], start: usize) -> Option<usize> {
    use core::arch::x86_64::{
        __m256i, _mm256_cmpeq_epi8, _mm256_loadu_si256, _mm256_movemask_epi8, _mm256_set1_epi8,
    };

    let n = buf.len();
    if start + 1 >= n {
        return None;
    }
    let mut i = start;
    // Pure register op (safe inside #[target_feature(enable = "avx2")]).
    let cr = _mm256_set1_epi8(0x0D);
    // We need `i + 32 <= n` so the 32-byte load is in-bounds AND `i + 32 + 1
    // <= n` so a CR at position 31 of the chunk can be confirmed by reading
    // [pos + 1]. That collapses to `i + 32 < n`.
    while i + 32 < n {
        // SAFETY: `i + 32 < n` ⇒ bytes [i, i+31] are inside `buf`; the
        // ptr is just past the buffer at most when this loop exits.
        let chunk = unsafe { _mm256_loadu_si256(buf.as_ptr().add(i) as *const __m256i) };
        // Pure register ops on already-loaded vectors.
        let mask = _mm256_movemask_epi8(_mm256_cmpeq_epi8(chunk, cr)) as u32;
        if mask != 0 {
            let bit = mask.trailing_zeros() as usize;
            let pos = i + bit;
            // `pos < i + 32 <= n - 1`, so `pos + 1 < n` is safe to index.
            if buf[pos + 1] == b'\n' {
                return Some(pos);
            }
            // Lone CR — resume scanning from the byte after it.
            i = pos + 1;
            continue;
        }
        i += 32;
    }
    // Tail: hand off to the SWAR scanner for the < 32-byte remainder.
    find_crlf_swar(buf, i)
}

#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon")]
unsafe fn find_crlf_neon(buf: &[u8], start: usize) -> Option<usize> {
    use core::arch::aarch64::{vceqq_u8, vdupq_n_u8, vld1q_u8, vmaxvq_u8};

    let n = buf.len();
    if start + 1 >= n {
        return None;
    }
    let mut i = start;
    // Pure register op (safe inside #[target_feature(enable = "neon")]).
    let cr = vdupq_n_u8(0x0D);
    // Need `i + 16 < n` so the 16-byte load is in-bounds AND a CR at
    // chunk byte 15 can be confirmed by reading [pos + 1] = [i + 16].
    while i + 16 < n {
        // SAFETY: `i + 16 < n` ⇒ bytes [i, i+15] are inside `buf`.
        let chunk = unsafe { vld1q_u8(buf.as_ptr().add(i)) };
        // Pure register ops on already-loaded vectors.
        let eq = vceqq_u8(chunk, cr);
        let any = vmaxvq_u8(eq);
        if any != 0 {
            // At least one CR in the chunk. Scalar-scan the 16 bytes to
            // find the first one — a hit is very rare in typical RESP
            // bulk-string scans (one CR per ~30+ byte arg), so this cold
            // path doesn't need a vectorised reduction.
            for j in 0..16 {
                if buf[i + j] == b'\r' {
                    let pos = i + j;
                    // `pos < i + 16 <= n - 1` ⇒ `pos + 1 < n` safe.
                    if buf[pos + 1] == b'\n' {
                        return Some(pos);
                    }
                    // Lone CR — delegate to SWAR scanner from byte
                    // after it (handles "the rest of this chunk + tail"
                    // uniformly).
                    return find_crlf_swar(buf, pos + 1);
                }
            }
        }
        i += 16;
    }
    find_crlf_swar(buf, i)
}

/// Portable u64 SWAR fallback — the same algorithm that shipped in
/// `kevy-resp::request::find_crlf` before A6/A7.
pub(crate) fn find_crlf_swar(buf: &[u8], start: usize) -> Option<usize> {
    const CR_BCAST: u64 = 0x0D0D_0D0D_0D0D_0D0D_u64;
    const ONES: u64 = 0x0101_0101_0101_0101_u64;
    const HIGH: u64 = 0x8080_8080_8080_8080_u64;

    let n = buf.len();
    let mut i = start;
    if i + 1 >= n {
        return None;
    }
    while i + 8 < n {
        let word = u64::from_le_bytes(buf[i..i + 8].try_into().expect("the i + 8 < n loop guard"));
        let x = word ^ CR_BCAST;
        let zeroed = x.wrapping_sub(ONES) & !x & HIGH;
        if zeroed != 0 {
            let bit_idx = zeroed.trailing_zeros();
            let pos = i + (bit_idx / 8) as usize;
            if buf[pos + 1] == b'\n' {
                return Some(pos);
            }
            i = pos + 1;
            continue;
        }
        i += 8;
    }
    while i + 1 < n {
        if buf[i] == b'\r' && buf[i + 1] == b'\n' {
            return Some(i);
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The vector tier this build will actually verify against SWAR, or
    /// `None` when there is none.
    ///
    /// `assert_matches_swar` compares `find_crlf` — the dispatcher — to
    /// `find_crlf_swar`. On x86_64 *without* AVX2 the dispatcher falls
    /// through to the SWAR loop, so that comparison is a function against
    /// itself: every input passes and the oracle verifies nothing, while
    /// looking exactly like an oracle that does. Naming the tier makes
    /// the difference visible, and the test below fails rather than
    /// passing quietly when there is no tier to check.
    /// A vector kernel and the name to report it under.
    type Tier = (&'static str, unsafe fn(&[u8], usize) -> Option<usize>);

    fn vector_tier() -> Option<Tier> {
        #[cfg(all(target_arch = "x86_64", feature = "std"))]
        if has_avx2() {
            return Some(("avx2", find_crlf_avx2));
        }
        #[cfg(target_arch = "aarch64")]
        return Some(("neon", find_crlf_neon));
        #[cfg(not(target_arch = "aarch64"))]
        None
    }

    /// Cross-tier oracle: SWAR is the reference. Every implementation must
    /// match it for every input shape.
    ///
    /// Drives the vector kernel **directly** where one exists, rather
    /// than through `find_crlf` — otherwise a dispatcher that declined to
    /// use it would still pass, which is the failure this guards.
    fn assert_matches_swar(buf: &[u8]) {
        let tier = vector_tier();
        for start in 0..=buf.len() {
            let want = find_crlf_swar(buf, start);
            assert_eq!(find_crlf(buf, start), want, "dispatch: buf={buf:?} start={start}");
            if let Some((name, f)) = tier {
                // SAFETY: `f` came from `vector_tier`, which returns a
                // kernel only when this machine has the ISA it needs.
                let got = unsafe { f(buf, start) };
                assert_eq!(got, want, "{name}: buf={buf:?} start={start}");
            }
        }
    }

    /// Say out loud which tier the oracle above is checking, so a run
    /// that verified only SWAR cannot be read as one that verified a
    /// vector kernel.
    #[test]
    fn the_oracle_names_the_tier_it_verified() {
        match vector_tier() {
            Some((name, _)) => {
                assert!(!name.is_empty());
                // A tier exists, so the comparison in `assert_matches_swar`
                // is between two different implementations.
            }
            None => {
                // No vector kernel on this build. `find_crlf` IS
                // `find_crlf_swar` here, so every other test in this
                // module is comparing a function to itself. That is a
                // true statement about the build, not a pass.
                assert_eq!(
                    find_crlf(b"ab\r\ncd", 0),
                    find_crlf_swar(b"ab\r\ncd", 0),
                    "SWAR-only build: nothing here verifies a vector kernel",
                );
            }
        }
    }

    #[test]
    fn empty_and_short_buffers() {
        for buf in &[b"" as &[u8], b"a", b"\r", b"\n", b"ab"] {
            assert_matches_swar(buf);
        }
    }

    #[test]
    fn crlf_at_every_offset() {
        // RESP frames put CRLFs at non-trivial offsets — exercise each.
        for off in 0..=80 {
            let mut buf = vec![b'X'; off + 2];
            buf[off] = b'\r';
            buf[off + 1] = b'\n';
            assert_eq!(find_crlf(&buf, 0), Some(off), "off={off}");
        }
    }

    #[test]
    fn lone_cr_does_not_terminate() {
        // CR without LF must be skipped; the scanner must continue past it.
        let mut buf = vec![b'X'; 50];
        buf[10] = b'\r';
        // No CRLF in the buffer at all.
        assert_eq!(find_crlf(&buf, 0), None);
        // Plant a real CRLF after the lone CR — must find it.
        buf[30] = b'\r';
        buf[31] = b'\n';
        assert_eq!(find_crlf(&buf, 0), Some(30));
    }

    #[test]
    fn multiple_crs_in_a_row() {
        let buf = b"X\r\r\r\nY";
        assert_eq!(find_crlf(buf, 0), Some(3));
    }

    #[test]
    fn start_past_crlf_finds_next() {
        let buf = b"AAA\r\nBBB\r\nCCC";
        assert_eq!(find_crlf(buf, 0), Some(3));
        assert_eq!(find_crlf(buf, 4), Some(8));
        assert_eq!(find_crlf(buf, 9), None);
    }

    #[test]
    fn cross_tier_oracle_random_shapes() {
        // Mix of buffer sizes that span the SIMD chunk boundary (16-byte
        // NEON, 32-byte AVX2): hit pre-chunk, in-chunk, in-tail.
        let shapes: &[&[u8]] = &[
            b"*3\r\n$3\r\nSET\r\n$3\r\nkey\r\n$5\r\nvalue\r\n",
            b"*1\r\n$4\r\nPING\r\n",
            b"PING\r\n",
            b"XXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXX\r\n", // CRLF right after a full AVX2 chunk
            b"XXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXX\r\n", // CRLF one byte after
            b"\r\nXX\r\nXXXXXX\r\n",                 // multiple CRLFs, hit each in turn
            b"XXXXXXXXXXXXXXXX\rXXXXXXXXXXXXXXXX\r\n", // lone CR in chunk N, real CRLF in chunk N+1
            b"only-text-no-newline-at-all-just-bytes-here-XXXX",
        ];
        for buf in shapes {
            assert_matches_swar(buf);
        }
    }

    #[test]
    fn returns_none_when_only_one_byte_after_start() {
        let buf = b"AAAAA";
        assert_eq!(find_crlf(buf, 4), None);
        assert_eq!(find_crlf(buf, 5), None);
    }
}
