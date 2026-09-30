use super::*;

fn keys(l: &Leaf, ov: &Overflow) -> Vec<Vec<u8>> {
    (0..l.len())
        .map(|i| {
            let mut k = Vec::new();
            l.key_into(i, ov, &mut k);
            k
        })
        .collect()
}

#[test]
fn a_leaf_is_one_1784_byte_allocation() {
    assert_eq!(std::mem::size_of::<Leaf>(), LEAF_BYTES);
}

#[test]
fn inserts_keep_slots_in_order_and_heads_decide_first() {
    let mut ov = Overflow::default();
    let mut l = Leaf::new(Shape { payloads: false, vlens: false });
    let ks: [&[u8]; 6] = [b"b", b"", b"a\0", b"a", b"abcdefghij", b"abcdefgh"];
    for k in ks {
        let at = l.lower_bound(&Probe::new(k), &ov);
        assert!(l.insert_at(at, Ent { key: k, vlen: 0, payload: &[] }, &mut ov));
    }
    let mut want: Vec<Vec<u8>> = ks.iter().map(|k| k.to_vec()).collect();
    want.sort();
    assert_eq!(keys(&l, &ov), want);
    assert_eq!(l.lower_bound(&Probe::new(b"a"), &ov), 1);
    assert_eq!(l.lower_bound(&Probe::past(b"a"), &ov), 5, "past a covers a, a\\0 and abc…");
    assert_eq!(l.lower_bound(&Probe::past(b"abcdefgh"), &ov), 5);
    assert_eq!(l.lower_bound(&Probe::new(b"abcdefghi"), &ov), 4);
}

#[test]
fn a_full_leaf_refuses_then_compacts_after_removals() {
    let mut ov = Overflow::default();
    let mut l = Leaf::new(Shape { payloads: true, vlens: false });
    let mut n = 0u32;
    while l.insert_at(
        l.len(),
        Ent { key: &n.to_be_bytes().repeat(3), vlen: 0, payload: b"pay" },
        &mut ov,
    ) {
        n += 1;
    }
    assert!(n > 50, "a leaf holds {n}");
    let before = l.len();
    for _ in 0..10 {
        l.remove_at(0, &mut ov);
    }
    assert!(
        l.insert_at(0, Ent { key: &[0; 12], vlen: 0, payload: b"pay" }, &mut ov),
        "dead bytes are reclaimed"
    );
    assert_eq!(l.len(), before - 9);
    assert_eq!(l.tail(0, &ov).payload, b"pay");
}

#[test]
fn big_entries_go_out_of_line_and_come_back() {
    let mut ov = Overflow::default();
    let mut l = Leaf::new(Shape { payloads: true, vlens: false });
    let big = vec![7u8; 5000];
    assert!(l.insert_at(0, Ent { key: &big, vlen: 0, payload: b"p" }, &mut ov));
    assert!(l.insert_at(1, Ent { key: &[8u8; 10], vlen: 0, payload: &vec![1u8; 3000] }, &mut ov));
    assert!(l.slab_of(0).is_some() && l.slab_of(1).is_some());
    assert_eq!(keys(&l, &ov)[0], big);
    assert_eq!(l.tail(1, &ov).payload.len(), 3000);
    assert_eq!(l.cmp_at(&Probe::new(&big), 0, &ov), Ordering::Equal);
    let mut other = Leaf::new(Shape { payloads: true, vlens: false });
    let n = l.len();
    l.move_span(0, n, &mut other, 0);
    assert_eq!((l.len(), other.len()), (0, 2), "a slab moves with its entry");
    assert_eq!(keys(&other, &ov)[0], big);
    other.remove_at(0, &mut ov);
    other.remove_head(1, &mut ov);
    assert_eq!(ov.bytes, 0, "every slab was released");
}

#[test]
fn moving_tails_and_dropping_heads_keeps_order() {
    let mut ov = Overflow::default();
    let mut a = Leaf::new(Shape { payloads: false, vlens: false });
    for i in 0..20u8 {
        a.insert_at(a.len(), Ent { key: &[i; 9], vlen: 0, payload: &[] }, &mut ov);
    }
    let mut b = Leaf::new(Shape { payloads: false, vlens: false });
    let n = a.len();
    a.move_span(15, n, &mut b, 0);
    a.remove_head(5, &mut ov);
    let firsts = |l: &Leaf| keys(l, &ov).iter().map(|k| k[0]).collect::<Vec<_>>();
    assert_eq!(firsts(&a), (5..15).collect::<Vec<_>>());
    assert_eq!(firsts(&b), (15..20).collect::<Vec<_>>());
    assert_eq!(a.span_bytes(0, a.len()), a.used());
}

#[test]
fn a_span_lands_before_or_after_what_the_leaf_holds() {
    let mut ov = Overflow::default();
    let shape = Shape { payloads: true, vlens: false };
    let (mut a, mut b) = (Leaf::new(shape), Leaf::new(shape));
    for i in 0..30u8 {
        let l = if i < 20 { &mut a } else { &mut b };
        l.insert_at(l.len(), Ent { key: &[i; 12], vlen: 0, payload: &[i; 5] }, &mut ov);
    }
    // a's last five go to b's front, then b's last two to a's end
    a.move_span(15, 20, &mut b, 0);
    let n = b.len();
    b.move_span(n - 2, n, &mut a, 15);
    let firsts = |l: &Leaf| keys(l, &ov).iter().map(|k| k[0]).collect::<Vec<_>>();
    assert_eq!(firsts(&a), [(0..15).collect::<Vec<_>>(), vec![28, 29]].concat());
    assert_eq!(firsts(&b), (15..28).collect::<Vec<_>>());
    assert!((0..b.len()).all(|i| b.tail(i, &ov).payload == [firsts(&b)[i]; 5]));
    assert_eq!(a.span_bytes(0, a.len()) + b.span_bytes(0, b.len()), a.used() + b.used());
}

#[test]
fn a_past_probe_sorts_after_everything_it_prefixes() {
    let heads = |k: &[u8]| (head_of(k), k.len(), k.get(8..).unwrap_or(&[]).to_vec());
    for (probe, key, want) in [
        (&b"ab"[..], &b"ab"[..], Ordering::Greater),
        (b"ab", b"abc", Ordering::Greater),
        (b"ab", b"a", Ordering::Greater),
        (b"ab", b"ac", Ordering::Less),
        (b"abcdefghij", b"abcdefghijk", Ordering::Greater),
        (b"abcdefghij", b"abcdefghik", Ordering::Less),
        (b"abcdefghij", b"abcdefgh", Ordering::Greater),
    ] {
        let (h, len, rest) = heads(key);
        assert_eq!(cmp_key(&Probe::past(probe), h, len, &rest), want, "{probe:?} vs {key:?}");
    }
}
