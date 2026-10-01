//! An entry's weight and access clock, kept beside the keyspace table.
//!
//! A key's weight is its key's heap plus its value's. Most values weigh
//! what their own few words say (a string's length, an integer's nothing),
//! so that part is worked out when asked. A collection's weight is a walk
//! over its members, so it is kept: in the low half of the slot's side word
//! (`KevyMap::enable_aux`), saturating at 4 GiB per key, and moved by every
//! change to the collection. The high half holds the LRU / LFU clock while
//! an eviction policy runs. Neither is in the slot itself, which keeps a
//! slot to one cache line; a GET or SET of a string never reads the side
//! word.
//!
//! The side lane exists once the table holds a collection or the clock
//! runs: every write of one of those makes sure of it first.

use crate::value::Value;

const LOW: u64 = 0xFFFF_FFFF;

/// Whether the value's weight is a walk over its members, and so is kept
/// in the side word rather than worked out.
#[inline]
pub(crate) fn kept(v: &Value) -> bool {
    matches!(
        v,
        Value::Hash(_)
            | Value::SegHash(_)
            | Value::List(_)
            | Value::SegList(_)
            | Value::Set(_)
            | Value::SegSet(_)
            | Value::ZSet(_)
            | Value::SegZSet(_)
            | Value::Stream(_)
    )
}

/// The value's weight: a kept one from the side word, any other worked out.
// every SET weighs the value it replaces and the one it writes: the three
// string forms are answered here, without the full per-type table
#[allow(clippy::inline_always)]
#[inline(always)]
pub(crate) fn value_weight(v: &Value, word: Option<u64>) -> u64 {
    match v {
        Value::Str(s) => s.heap_bytes() as u64,
        Value::Int(_) => 0,
        Value::ArcBulk(a) => a.len() as u64,
        v if kept(v) => word.map_or(0, |w| w & LOW),
        v => v.weight(),
    }
}

/// Record `weight` as a value's weight in its side word: kept when the
/// value is one whose weight is [`kept`], zero for one worked out when
/// asked. The clock half stays.
#[inline]
pub(crate) fn stamp(word: &mut u64, is_kept: bool, weight: u64) {
    let low = if is_kept { weight.min(LOW) } else { 0 };
    *word = (*word & !LOW) | low;
}

/// The kept-weight half of a side word (zero for a value worked out).
#[inline]
pub(crate) fn kept_half(word: Option<u64>) -> u64 {
    word.map_or(0, |w| w & LOW)
}

/// Move a kept weight by `delta`, saturating at zero and at the ceiling.
#[inline]
pub(crate) fn shift(word: &mut u64, delta: i64) {
    let low = (*word & LOW).saturating_add_signed(delta).min(LOW);
    *word = (*word & !LOW) | low;
}

/// The LRU / LFU clock in a side word.
#[inline]
pub(crate) fn clock(word: Option<u64>) -> u32 {
    word.map_or(0, |w| (w >> 32) as u32)
}

/// Set the clock half of a side word.
#[inline]
pub(crate) fn set_clock(word: &mut u64, c: u32) {
    *word = (*word & LOW) | (u64::from(c) << 32);
}

#[cfg(test)]
impl crate::Store {
    /// The weight the store holds `key` at: its key's heap plus its value's
    /// as recorded (kept) or worked out.
    pub(crate) fn weight_of(&self, key: &[u8]) -> Option<u64> {
        let slot = self.map.find_slot(key)?;
        let (k, e) = self.map.slot(slot)?;
        Some(k.heap_bytes() as u64 + value_weight(&e.value, self.map.aux(slot)))
    }

    /// The access clock recorded for `key`.
    pub(crate) fn clock_of(&self, key: &[u8]) -> Option<u32> {
        let slot = self.map.find_slot(key)?;
        Some(clock(self.map.aux(slot)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Store;

    /// Every key's recorded weight is what a full walk gives, and the
    /// store's total is the sum plus the table.
    fn exact(s: &Store) {
        let mut sum = s.keyspace_bytes;
        for k in s.map.keys() {
            let (_, e) = s.map.slot(s.map.find_slot(k).unwrap()).unwrap();
            let full = k.heap_bytes() as u64 + e.value.weight();
            assert_eq!(s.weight_of(k.as_slice()), Some(full), "{k:?}");
            sum += full;
        }
        assert_eq!(s.used_memory(), sum);
    }

    #[test]
    fn kept_weights_follow_every_change_and_every_change_of_form() {
        let mut s = Store::new();
        let mut x = 0x2545_f491u64;
        let mut next = |n: u64| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x % n
        };
        for step in 0..20_000u64 {
            let key = format!("k{}", next(40));
            let member = format!("member-{}-{}", next(300), "x".repeat(next(40) as usize));
            let (k, m) = (key.as_bytes(), member.as_bytes());
            // each key keeps one type: a WRONGTYPE answer is a no-op here
            let _ = match k[1] % 6 {
                0 => s.hset(k, &[(m, b"v")]).map(drop),
                1 => s.hdel(k, &[m]).map(drop),
                2 => s.rpush(k, &[m]).map(drop),
                3 => s.lpop(k, 1 + next(3) as usize).map(drop),
                4 => s.sadd(k, &[m]).map(drop),
                _ => s.srem(k, &[m]).map(drop),
            };
            if step % 3 == 0 {
                let z = format!("z{}", next(10));
                if next(3) == 0 {
                    let _ = s.zrem(z.as_bytes(), &[m]);
                } else {
                    let _ = s.zadd(z.as_bytes(), &[(next(100) as f64, m)]);
                }
            }
            if step % 997 == 0 {
                s.del(&[k]);
            }
            if step % 500 == 0 {
                exact(&s);
            }
        }
        exact(&s);
    }

    #[test]
    fn segmented_forms_are_charged_exactly_through_promotion_and_splits() {
        let mut s = Store::new();
        let mut x = 0x9e37_79b9u64;
        let mut next = |n: u64| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x % n
        };
        // past every promotion threshold (16 Ki members) and many splits
        for step in 0..160_000u64 {
            let m = format!("m{}-{}", next(40_000), "y".repeat(next(30) as usize));
            let m = m.as_bytes();
            let removing = next(5) == 0;
            let _ = match step % 4 {
                0 if removing => s.hdel(b"h", &[m]).map(drop),
                0 => s.hset(b"h", &[(m, b"v")]).map(drop),
                1 if removing => s.srem(b"s", &[m]).map(drop),
                1 => s.sadd(b"s", &[m]).map(drop),
                2 if removing => s.zrem(b"z", &[m]).map(drop),
                2 => s.zadd(b"z", &[(next(1_000) as f64, m)]).map(drop),
                _ if removing => s.lpop(b"l", 1).map(drop),
                _ => s.rpush(b"l", &[m]).map(drop),
            };
            if step % 16_000 == 0 {
                exact(&s);
            }
        }
        exact(&s);
        let forms: Vec<&str> = [b"h".as_slice(), b"s", b"z", b"l"]
            .iter()
            .map(|k| match &s.map.get(*k).unwrap().value {
                crate::Value::SegHash(_) => "seghash",
                crate::Value::SegSet(_) => "segset",
                crate::Value::SegZSet(_) => "segzset",
                crate::Value::SegList(_) => "seglist",
                _ => "flat",
            })
            .collect();
        assert_eq!(forms, ["seghash", "segset", "segzset", "seglist"], "every form was reached");
    }

    #[test]
    fn a_string_over_a_collection_and_back_is_charged_exactly() {
        let mut s = Store::new();
        s.hset(b"k", &[(b"f".as_slice(), [b'v'; 200].as_slice())]).unwrap();
        exact(&s);
        s.set(b"k", vec![b's'; 100], None, crate::SetCondition::Always);
        exact(&s);
        s.append(b"k", &[b'a'; 50]).unwrap();
        exact(&s);
        s.del(&[b"k".as_slice()]);
        s.sadd(b"k", &[b"m".as_slice()]).unwrap();
        exact(&s);
    }

    #[test]
    fn a_value_loaded_over_a_collection_frees_what_the_collection_weighed() {
        let mut s = Store::new();
        for i in 0..50u32 {
            s.rpush(b"list", &[format!("item-{i}-{}", "z".repeat(40)).as_bytes()]).unwrap();
        }
        exact(&s);
        // a load meeting the key again overwrites it in place: what the
        // slot's word said the list weighed is what leaves
        s.insert_loaded(b"list".to_vec(), crate::Value::Int(7), None);
        exact(&s);
    }

    #[test]
    fn the_halves_do_not_disturb_each_other() {
        let mut w = 0u64;
        set_clock(&mut w, 0xDEAD_BEEF);
        shift(&mut w, 100);
        shift(&mut w, -30);
        assert_eq!((w & LOW, clock(Some(w))), (70, 0xDEAD_BEEF));
        shift(&mut w, -1_000);
        assert_eq!(w & LOW, 0, "saturates at zero");
        shift(&mut w, i64::MAX);
        assert_eq!((w & LOW, clock(Some(w))), (LOW, 0xDEAD_BEEF), "and at the ceiling");
    }
}
