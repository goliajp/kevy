//! The side lane against a model: every live entry's word is the one last
//! written for its key, through inserts, removals, regrowth and clones.

use std::collections::HashMap;

use crate::KevyMap;

struct Rng(u64);

impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % n
    }
}

fn check(m: &KevyMap<u64, u64>, model: &HashMap<u64, u64>) {
    assert_eq!(m.len(), model.len());
    for (k, want) in model {
        let slot = m.find_slot(k).expect("live key has a slot");
        assert_eq!(m.aux(slot), Some(*want), "key {k}");
    }
}

#[test]
fn every_word_follows_its_key_through_inserts_removes_and_growth() {
    let mut r = Rng(0x9e37_79b9);
    let mut m: KevyMap<u64, u64> = KevyMap::new();
    m.enable_aux();
    let mut model: HashMap<u64, u64> = HashMap::new();
    for step in 0..60_000u64 {
        let k = r.below(4_000);
        match r.below(4) {
            0 | 1 => {
                // a new key starts at zero; an overwrite keeps its word
                if m.insert(k, step).is_none() {
                    model.insert(k, 0);
                }
            }
            2 => {
                if m.remove(&k).is_some() {
                    model.remove(&k);
                }
            }
            _ => {
                if let Some(slot) = m.find_slot(&k) {
                    *m.aux_mut(slot).expect("kept") = step;
                    model.insert(k, step);
                }
            }
        }
        if step % 5_000 == 0 {
            check(&m, &model);
        }
    }
    check(&m, &model);
    check(&m.clone(), &model);
}

#[test]
fn a_reused_slot_does_not_inherit_the_word_of_the_key_before_it() {
    let mut m: KevyMap<u64, ()> = KevyMap::new();
    m.enable_aux();
    m.insert(1, ());
    let slot = m.find_slot(&1).unwrap();
    *m.aux_mut(slot).unwrap() = 99;
    m.remove(&1);
    assert_eq!(m.aux(slot), None, "an emptied slot has no word");
    m.insert(1, ());
    let again = m.find_slot(&1).unwrap();
    assert_eq!(m.aux(again), Some(0));
}

#[test]
fn without_a_lane_there_are_no_words_and_no_bytes() {
    let mut m: KevyMap<u64, u64> = KevyMap::new();
    for i in 0..100 {
        m.insert(i, i);
    }
    let slot = m.find_slot(&5).unwrap();
    assert_eq!((m.aux(slot), m.aux_mut(slot).is_some()), (None, false));
    let bare = m.footprint();
    m.enable_aux();
    assert_eq!(m.footprint(), bare + crate::malloc_footprint(m.capacity() * 8));
    assert_eq!(m.aux(slot), Some(0));
}
