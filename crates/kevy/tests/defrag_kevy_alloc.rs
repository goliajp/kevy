//! The defrag pass end to end, under the allocator it is built for.
//!
//! Rows shaped like a tiered table's (five fields, one of 900 bytes) are
//! loaded, a third of them removed at random the way demotion frees them,
//! and the store's defrag step run beside kevy-alloc's reclaim. The heap's
//! free space inside its spans has to fall to a small share of what is
//! live: that is the memory a tiered server could not give back before.

#![cfg(feature = "kevy-alloc")]

use std::alloc::Layout;

use kevy_store::Store;

#[global_allocator]
static GLOBAL: kevy_alloc::KevyAlloc = kevy_alloc::KevyAlloc;

fn hint(ptr: *const u8, size: usize, align: usize) -> bool {
    Layout::from_size_align(size, align).is_ok_and(|l| kevy_alloc::global::should_move(ptr, l))
}

fn free_share() -> (u64, f64) {
    let a = kevy_alloc::thread_stats().expect("this thread's heap");
    (a.span_free, a.span_free as f64 / (a.live + a.rounding) as f64)
}

#[test]
fn the_pass_packs_a_heap_that_demotion_left_full_of_holes() {
    let mut s = Store::new();
    let pad = vec![b'p'; 900];
    let rows = 60_000u32;
    for i in 0..rows {
        let (k, n) = (format!("row:{i}"), i.to_string());
        let fields: [(&[u8], &[u8]); 5] = [
            (b"id", n.as_bytes()),
            (b"status", b"paid"),
            (b"score", n.as_bytes()),
            (b"ts", b"1790000000"),
            (b"pad", &pad),
        ];
        s.hset(k.as_bytes(), &fields).unwrap();
    }
    let mut x = 0x9E37_79B9_7F4A_7C15u64;
    let mut kept = Vec::new();
    for i in 0..rows {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        if x.is_multiple_of(3) {
            s.del(&[format!("row:{i}").as_bytes()]);
        } else {
            kept.push(i);
        }
    }
    kevy_alloc::thread_reclaim();
    let (before, share_before) = free_share();
    assert!(share_before > 0.15, "the removals left holes to pack: {share_before:.3}");

    s.set_defrag_hint(Some(hint));
    for _ in 0..40 {
        let mut lap_moved = 0;
        loop {
            let step = s.defrag_step(4_096);
            lap_moved += step.moved;
            if step.lap_done {
                break;
            }
        }
        kevy_alloc::thread_reclaim();
        if lap_moved == 0 {
            break;
        }
    }
    let (after, share_after) = free_share();
    eprintln!("span_free {before} -> {after} ({share_before:.3} -> {share_after:.3} of live)");
    assert!(share_after < 0.05, "free space still {share_after:.3} of live after the pass");
    for i in kept {
        let k = format!("row:{i}");
        assert_eq!(s.hlen(k.as_bytes()).unwrap(), 5, "{k}");
        assert_eq!(s.hget(k.as_bytes(), b"pad").unwrap(), Some(&pad[..]), "{k}");
    }
}
