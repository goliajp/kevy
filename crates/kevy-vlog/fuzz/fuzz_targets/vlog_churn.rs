//! Fuzz the vlog's full lifecycle on an arbitrary-input-driven op
//! sequence: append (arbitrary key/payload sizes), overwrite (old ref
//! dies), delete, read-back, and compaction — with a tiny rotation
//! threshold so file sealing and retirement happen constantly.
//!
//! Invariants asserted on every input:
//!   * no panic, no OOM, every io::Result is Ok on the happy path
//!   * a live ref ALWAYS reads back its exact key + payload
//!   * compaction never loses a live record, never resurrects a dead one
//!   * stats stay coherent (live <= bytes)

#![no_main]

use std::collections::HashMap;

use libfuzzer_sys::fuzz_target;
use kevy_vlog::{CompactOwner, Vlog, VlogRef};

struct Owner {
    live: HashMap<Vec<u8>, (VlogRef, u8)>, // key -> (ref, fill byte)
}
impl CompactOwner for Owner {
    fn is_live(&mut self, key: &[u8], old: VlogRef) -> bool {
        self.live.get(key).map(|(r, _)| *r) == Some(old)
    }
    fn moved(&mut self, key: &[u8], old: VlogRef, new: VlogRef) {
        let e = self.live.get_mut(key).expect("moved() for a key we said was live");
        assert_eq!(e.0, old);
        e.0 = new;
    }
}

fuzz_target!(|data: &[u8]| {
    let dir = std::env::temp_dir().join(format!(
        "kevy-vlog-fuzz-{}-{:x}",
        std::process::id(),
        data.iter().fold(0u64, |h, b| h.wrapping_mul(31).wrapping_add(*b as u64))
    ));
    let Ok(mut v) = Vlog::open(&dir, 384) else { return };
    let mut owner = Owner { live: HashMap::new() };

    let mut it = data.iter().copied();
    while let Some(op) = it.next() {
        let kn = op % 16; // small key universe => overwrites are common
        let key = format!("k{kn}").into_bytes();
        match op % 5 {
            0 | 1 => {
                let fill = it.next().unwrap_or(0);
                let plen = usize::from(it.next().unwrap_or(0)) * 3; // 0..=765
                let r = v.append(&key, &vec![fill; plen]).unwrap();
                if let Some((old, _)) = owner.live.insert(key, (r, fill)) {
                    v.note_dead(old);
                }
            }
            2 => {
                if let Some((old, _)) = owner.live.remove(&key) {
                    v.note_dead(old);
                }
            }
            3 => {
                if let Some((r, fill)) = owner.live.get(&key) {
                    let (rk, rp) = v.read(*r).unwrap();
                    assert_eq!(rk, key);
                    assert!(rp.iter().all(|b| b == fill));
                }
            }
            _ => {
                v.compact_below(u32::from(it.next().unwrap_or(50) % 102), &mut owner).unwrap();
            }
        }
        let s = v.stats();
        assert!(s.live_bytes <= s.bytes);
    }
    // Terminal check: every live ref still round-trips.
    for (key, (r, fill)) in &owner.live {
        let (rk, rp) = v.read(*r).unwrap();
        assert_eq!(&rk, key);
        assert!(rp.iter().all(|b| b == fill));
    }
    let _ = std::fs::remove_dir_all(&dir);
});
