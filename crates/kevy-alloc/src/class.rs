//! Size classes.
//!
//! # Why eight subdivisions per octave, not four
//!
//! Two terms of the accounting contract pull against each other: finer
//! classes cut **rounding**, coarser classes cut **span slack** (fewer
//! classes means fewer partial spans sitting around). Both are real, so
//! the choice is not taste.
//!
//! §8.1 of the RFC settles it. Rounding is the only term that scales
//! with the dataset; slack is O(classes × shards) and constant in the
//! data. **Spend the constant to shrink the term that scales.** Hence
//! eight subdivisions per octave — worst-case rounding ≈ 11.1 %, against
//! the ~20 % that four subdivisions would give.
//!
//! Below 128 bytes the classes step by 8, and the bound there is
//! **absolute rather than relative**: at most 7 bytes wasted, but that
//! is 29 % of a 24-byte class. The relative bound cannot be rescued at
//! that end — 8 bytes is the granularity floor, since finer classes
//! would not keep slots 8-byte aligned. Saying "the step is finer so it
//! costs nothing" would have been wrong, and the class table's own test
//! said so before this comment was written.
//!
//! # Why spans are one uniform size after all
//!
//! The first draft sized spans per class, reasoning that a fixed span
//! size makes slack proportional to the class count — which the decision
//! above deliberately increases. Two things overturned it.
//!
//! Geometry: `dealloc` finds a pointer's span by masking, which needs
//! uniform span geometry. Variable spans would need a lookup structure
//! on the free path, paid on every deallocation, to save address space.
//!
//! And the worry was misplaced. Spans hand out slots by bumping a
//! cursor, so the untouched tail of a span is **mapped but never
//! resident** — it costs address space, not memory. That is why the
//! accounting splits slack into touched (`span_free`) and untouched
//! (`virgin`): only the first is RSS. A large uniform span whose tail is
//! never reached is close to free in the metric that matters.

/// The largest allocation served by a size class. Above this, requests
/// are mapped directly and returned with `unmap`.
///
/// Raised 8 KiB → 32 KiB by the M1 decomposition. The old cap assumed
/// large allocations were infrequent — and under a cross-shard load the
/// dispatch and reply buffers sit just past 8 KiB, so every one paid an
/// mmap on birth and a munmap on death, eight shards serialised on the
/// process-wide mmap_lock: **40 % of server self time** was
/// `__x64_sys_munmap` + `vm_mmap_pgoff` (finding
/// measured (the mmap-lock convoy finding). glibc recycles those
/// buffers from its arena with zero syscalls, which is the entire
/// cross-shard gap. A 64 KiB span still holds 2–8 slots at these sizes.
pub const MAX_SMALL: usize = 32_768;

/// Alignment every class satisfies natively, because every class is a
/// multiple of it and spans are aligned far beyond it.
pub const MIN_ALIGN: usize = 8;

/// Strongest alignment served by picking a suitable class rather than by
/// over-allocating. Requests above this go to the `GlobalAlloc` shim's
/// over-aligned path.
///
/// 16 matters enough to be worth serving directly — `u128`, `AtomicU64`
/// pairs and most SIMD vectors ask for it — and it costs only skipping
/// to the next class when the natural one is not a multiple of 16.
pub const MAX_NATIVE_ALIGN: usize = 16;

/// Every span is this many bytes, whatever class it serves. Uniform
/// geometry is what lets `dealloc` find a span by masking; see the
/// module docs for why the variable-size draft lost. 64 KiB gives the
/// largest class eight slots and the smallest four thousand.
pub const SPAN_BYTES: usize = 64 * 1024;

/// The class table: every size a slot may have, ascending.
///
/// Written out rather than generated so it can be read and checked. A
/// stone's most important property is that a reviewer can see what it
/// does; 79 numbers are cheaper to audit than the loop that would emit
/// them.
pub const CLASSES: [u32; 79] = [
    // 16..=128 step 8 — finer than the octave rule, and free.
    16, 24, 32, 40, 48, 56, 64, 72, 80, 88, 96, 104, 112, 120, 128, // 128..=256 step 16
    144, 160, 176, 192, 208, 224, 240, 256, // 256..=512 step 32
    288, 320, 352, 384, 416, 448, 480, 512, // 512..=1024 step 64
    576, 640, 704, 768, 832, 896, 960, 1024, // 1024..=2048 step 128
    1152, 1280, 1408, 1536, 1664, 1792, 1920, 2048, // 2048..=4096 step 256
    2304, 2560, 2816, 3072, 3328, 3584, 3840, 4096, // 4096..=8192 step 512
    4608, 5120, 5632, 6144, 6656, 7168, 7680, 8192, // 8192..=16384 step 1024
    9216, 10240, 11264, 12288, 13312, 14336, 15360, 16384, // 16384..=32768 step 2048
    18432, 20480, 22528, 24576, 26624, 28672, 30720, 32768,
];

/// Number of size classes.
pub const NCLASSES: usize = CLASSES.len();

/// Lookup granularity: one table entry per 8 bytes of request size.
const GRAIN: usize = 8;
const LOOKUP_LEN: usize = MAX_SMALL / GRAIN + 1;

/// `size -> class index`, resolved by table rather than by arithmetic so
/// the hot path is one load and no branching over the octave structure.
static LOOKUP: [u8; LOOKUP_LEN] = build_lookup();

const fn build_lookup() -> [u8; LOOKUP_LEN] {
    let mut table = [0u8; LOOKUP_LEN];
    let mut i = 0;
    while i < LOOKUP_LEN {
        let size = i * GRAIN;
        let mut c = 0;
        while c < NCLASSES {
            if CLASSES[c] as usize >= size {
                break;
            }
            c += 1;
        }
        table[i] = c as u8;
        i += 1;
    }
    table
}

/// The class index serving `size` at `align`, or `None` when the request
/// belongs on the direct-mapping path (too large, or too strictly
/// aligned to serve from a class).
///
/// Slots sit at multiples of the class size inside a span, and span
/// bases are 64 KiB-aligned, so a slot's alignment is exactly the
/// alignment of its class size. Serving a 16-byte alignment therefore
/// means choosing a class that is a multiple of 16 — which, in the
/// 8-stepped region below 128, is always the very next one.
/// # Examples
///
/// ```
/// use kevy_alloc::class::{index_of, size_of};
///
/// // Every served size rounds UP to its class, never down.
/// let i = index_of(1, 1).unwrap();
/// assert!(size_of(i) >= 1);
/// let i = index_of(100, 1).unwrap();
/// assert!(size_of(i) >= 100);
///
/// // A strict alignment picks a class that is a multiple of it.
/// let i = index_of(24, 16).unwrap();
/// assert_eq!(size_of(i) % 16, 0);
///
/// // Too large, or too strictly aligned, is None — the direct-mapping
/// // path, not a wrong class.
/// assert_eq!(index_of(1 << 30, 1), None);
/// ```
#[inline]
#[must_use]
pub fn index_of(size: usize, align: usize) -> Option<usize> {
    if size > MAX_SMALL || align > MAX_NATIVE_ALIGN {
        return None;
    }
    let base = LOOKUP[size.div_ceil(GRAIN)] as usize;
    if align <= MIN_ALIGN || CLASSES[base].is_multiple_of(align as u32) {
        return Some(base);
    }
    // Only the 8-stepped region can miss, and there the next class up is
    // always a multiple of 16.
    let next = base + 1;
    debug_assert!(next < NCLASSES && CLASSES[next].is_multiple_of(align as u32));
    Some(next)
}

/// Slot size for a class index.
/// # Examples
///
/// ```
/// use kevy_alloc::class::{index_of, size_of};
/// // Classes ascend, so a bigger request never lands in a smaller slot.
/// let small = size_of(index_of(8, 1).unwrap());
/// let big = size_of(index_of(200, 1).unwrap());
/// assert!(small <= big);
/// ```
#[inline]
#[must_use]
pub fn size_of(index: usize) -> usize {
    CLASSES[index] as usize
}

/// `ceil(2^32 / size)` per class — the reciprocal that turns the free
/// path's slot-index division into a multiply-shift.
///
/// Exactness (Granlund–Montgomery): with `m = ceil(2^32 / d)`,
/// `(n * m) >> 32 == n / d` for every `n` where
/// `n * (m*d − 2^32) < 2^32`. Here `m*d − 2^32 < d ≤ 2^15` and
/// `n < SPAN_BYTES = 2^16`, so the error product stays below `2^31`.
/// The unit test still checks every class at every span offset —
/// exhaustively, because a proof in a comment has no CI.
const RECIP: [u32; NCLASSES] = {
    let mut t = [0u32; NCLASSES];
    let mut i = 0;
    while i < NCLASSES {
        t[i] = ((1u64 << 32).div_ceil(CLASSES[i] as u64)) as u32;
        i += 1;
    }
    t
};

/// Divide a span offset by a class's slot size via the reciprocal
/// table. `off` must be below [`SPAN_BYTES`].
#[inline]
#[must_use]
pub fn slot_of_offset(off: usize, index: usize) -> u32 {
    ((off as u64 * RECIP[index] as u64) >> 32) as u32
}

/// Slots that fit in a span of this class.
#[must_use]
pub const fn slots_per_span(index: usize) -> usize {
    SPAN_BYTES / CLASSES[index] as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classes_ascend_and_are_natively_aligned() {
        let mut prev = 0u32;
        for (i, &c) in CLASSES.iter().enumerate() {
            assert!(c > prev, "class {i} = {c} does not exceed {prev}");
            assert!(
                (c as usize).is_multiple_of(MIN_ALIGN),
                "class {c} is not {MIN_ALIGN}-byte aligned"
            );
            prev = c;
        }
    }

    #[test]
    fn the_rounding_bound_is_relative_above_128_and_absolute_below() {
        // The octave decision claims a relative bound; the 8-stepped
        // region cannot honour one (8 bytes is 33 % of a 24-byte class)
        // and is bounded absolutely instead. Both halves are asserted so
        // neither claim can quietly weaken.
        for w in CLASSES.windows(2) {
            let (prev, cur) = (w[0], w[1]);
            let worst = cur - (prev + 1);
            if cur >= 128 {
                let rel = f64::from(worst) / f64::from(cur);
                assert!(rel < 0.125, "class {prev} -> {cur} wastes {:.1}%", rel * 100.0);
            } else {
                assert!(worst < GRAIN as u32, "class {prev} -> {cur} wastes {worst} bytes");
            }
        }
    }

    #[test]
    fn lookup_picks_the_smallest_class_that_fits() {
        for size in 1..=MAX_SMALL {
            let idx = index_of(size, 1).expect("within the small range");
            let picked = size_of(idx);
            assert!(picked >= size, "class {picked} too small for {size}");
            if idx > 0 {
                assert!(
                    size_of(idx - 1) < size,
                    "class {} would also have fit {size}",
                    size_of(idx - 1)
                );
            }
        }
    }

    #[test]
    fn sixteen_byte_alignment_is_served_by_class_choice() {
        for size in 1..=MAX_SMALL {
            let idx = index_of(size, 16).expect("16 is served natively");
            let picked = size_of(idx);
            assert!(picked >= size, "class {picked} too small for {size}");
            assert!(picked.is_multiple_of(16), "class {picked} cannot align {size} to 16");
        }
    }

    #[test]
    fn requests_off_the_class_path_have_no_class() {
        assert!(index_of(MAX_SMALL + 1, 1).is_none());
        assert!(index_of(usize::MAX, 1).is_none());
        assert!(index_of(64, 32).is_none(), "over-alignment belongs to the shim");
    }

    #[test]
    fn every_span_holds_at_least_a_few_slots() {
        for i in 0..NCLASSES {
            let slots = slots_per_span(i);
            // The 16-32 KiB classes get 2-4 slots per span — few, but a
            // span with two slots still reclaims page-granularly, and
            // the alternative was an mmap/munmap pair per buffer.
            assert!(slots >= 2, "class {} gets only {slots} slots per span", size_of(i));
        }
        assert_eq!(SPAN_BYTES % crate::os::PAGE, 0, "a span must be a whole number of pages");
        assert!(SPAN_BYTES.is_power_of_two(), "masking needs a power-of-two span");
    }

    /// The reciprocal shortcut must equal the division at every span
    /// offset of every class — exhaustive, not sampled: 64 Ki offsets
    /// x 79 classes is five million cheap checks, and the proof in the
    /// table's comment has no CI without this.
    #[test]
    fn the_reciprocal_agrees_with_division_everywhere() {
        for c in 0..NCLASSES {
            let size = size_of(c);
            for off in 0..SPAN_BYTES {
                assert_eq!(
                    slot_of_offset(off, c) as usize,
                    off / size,
                    "class {c} (size {size}) at offset {off}"
                );
            }
        }
    }
}
