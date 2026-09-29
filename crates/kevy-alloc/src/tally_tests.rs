use core::ptr::NonNull;

use crate::heap::Heap;
use crate::os;

/// A small class, a page-straddling one, a multi-page one, and the
/// largest, so spans of every shape empty, refill and lose pages.
const SIZES: [usize; 9] = [16, 24, 100, 400, 912, 3_000, 4_000, 20_000, 32_768];

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

fn agree(h: &Heap, step: usize, what: &str) {
    let (fast, walked) = (h.snapshot(), h.snapshot_walked());
    assert_eq!(fast, walked, "step {step} after {what}");
    assert!(fast.balanced(), "step {step} after {what}: {fast:?}");
}

/// `a` owns `held`; `b` owns `theirs`. Every step does one thing to one
/// of them, then both heaps' fast snapshots must equal the walk.
fn churn(seed: u64, steps: usize) {
    let (mut a, mut b) = (Heap::new(1), Heap::new(2));
    let mut held: Vec<(NonNull<u8>, usize)> = Vec::new();
    let mut theirs: Vec<(NonNull<u8>, usize)> = Vec::new();
    let mut rng = Rng(seed);
    for step in 0..steps {
        let r = rng.next();
        let pick = (r >> 16) as usize;
        let what = match r % 20 {
            0..=6 => {
                let size = SIZES[pick % SIZES.len()] - (pick >> 8) % 8;
                held.push((a.alloc(size, 8).expect("mapped"), size));
                "alloc"
            }
            7..=9 if !held.is_empty() => {
                let (p, size) = held.swap_remove(pick % held.len());
                // SAFETY: allocated by `a` with this size and alignment.
                unsafe { a.dealloc(p, size, 8) };
                "local free"
            }
            10 | 11 if !held.is_empty() => {
                let (p, size) = held.swap_remove(pick % held.len());
                // SAFETY: as above; `b` routes it home through its outbound ring.
                unsafe { b.dealloc(p, size, 8) };
                "foreign free"
            }
            12 => {
                let size = SIZES[pick % SIZES.len()];
                theirs.push((b.alloc(size, 8).expect("mapped"), size));
                "alloc on b"
            }
            13 if !theirs.is_empty() => {
                let (p, size) = theirs.swap_remove(pick % theirs.len());
                // SAFETY: allocated by `b` with this size; `a` ships it home.
                unsafe { a.dealloc(p, size, 8) };
                "foreign free into b"
            }
            14 if !held.is_empty() => {
                let i = pick % held.len();
                let (p, size) = held[i];
                if !a.should_move(p.as_ptr(), size, 8) {
                    continue;
                }
                let q = a.alloc(size, 8).expect("mapped");
                // SAFETY: both live from `a` with this size; `p` is released after the copy.
                unsafe {
                    core::ptr::copy_nonoverlapping(p.as_ptr(), q.as_ptr(), size);
                    a.dealloc(p, size, 8);
                }
                held[i] = (q, size);
                "defrag move"
            }
            15 if !held.is_empty() => {
                let i = pick % held.len();
                let (p, size) = held[i];
                let to = size - size.min(3) + 1;
                // SAFETY: `p` is live from `a` with `size`.
                if unsafe { a.try_resize_in_place(p, size, to, 8) } {
                    held[i].1 = to;
                }
                "resize in place"
            }
            16 => {
                b.reclaim();
                "b reclaims (ships a's frees)"
            }
            17 => {
                a.drain_foreign();
                "drain"
            }
            18 => {
                a.reclaim_with(pick.is_multiple_of(2));
                "reclaim"
            }
            _ => {
                a.flush_claims();
                "flush claims"
            }
        };
        agree(&a, step, what);
        agree(&b, step, what);
    }
    for (p, size) in held.drain(..) {
        // SAFETY: as above.
        unsafe { a.dealloc(p, size, 8) };
    }
    for (p, size) in theirs.drain(..) {
        // SAFETY: as above.
        unsafe { b.dealloc(p, size, 8) };
    }
    b.reclaim();
    a.reclaim();
    agree(&a, steps, "teardown");
    agree(&b, steps, "teardown");
    assert_eq!(a.snapshot().live, 0);
}

#[test]
fn the_running_totals_equal_the_walk_after_every_step() {
    if !os::available() {
        eprintln!("skipped: no anonymous mapping on this target");
        return;
    }
    for seed in [0x9E37_79B9_7F4A_7C15, 0xD1B5_4A32_D192_ED03, 0x2545_F491_4F6C_DD1D, 7] {
        churn(seed, 6_000);
    }
}

/// Fill spans, empty most of them, reclaim with discards so live spans
/// lose pages, then refill into the returned pages: the path where a
/// claim un-returns slots, which random churn reaches only rarely.
#[test]
fn a_claim_into_returned_pages_matches_the_walk() {
    if !os::available() {
        eprintln!("skipped: no anonymous mapping on this target");
        return;
    }
    for size in SIZES {
        let mut h = Heap::new(3);
        let n = 3 * crate::class::SPAN_BYTES / size;
        let held: Vec<_> = (0..n).map(|_| h.alloc(size, 8).expect("mapped")).collect();
        for (i, p) in held.iter().enumerate() {
            if !i.is_multiple_of(7) {
                // SAFETY: allocated by `h` with this size.
                unsafe { h.dealloc(*p, size, 8) };
            }
        }
        let keep: Vec<_> =
            held.iter().enumerate().filter(|(i, _)| i.is_multiple_of(7)).map(|(_, p)| *p).collect();
        h.reclaim_with(true);
        agree(&h, 0, "reclaim with discards");
        let again: Vec<_> = (0..n / 2).map(|_| h.alloc(size, 8).expect("mapped")).collect();
        agree(&h, 1, "refill into returned pages");
        for p in keep.into_iter().chain(again) {
            // SAFETY: allocated by `h` with this size.
            unsafe { h.dealloc(p, size, 8) };
        }
        h.reclaim_with(false);
        agree(&h, 2, "teardown");
    }
}
