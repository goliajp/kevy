//! Packed-row layout and cost tests.

use super::*;

/// `n` throwaway column names — the tests are about the payload layout,
/// not about what the columns are called.
fn names(n: usize) -> ColumnNames {
    (0..n).map(|i| format!("c{i}").into_bytes()).collect()
}

#[test]
fn round_trips_every_column_including_absent_and_empty() {
    let cols: Vec<Option<&[u8]>> =
        vec![Some(&b"id7"[..]), None, Some(&b""[..]), Some(&b"a longer value"[..])];
    let r = PackedRow::build(&names(cols.len()), &cols).expect("fits");
    assert_eq!(r.columns(), 4);
    for (i, want) in cols.iter().enumerate() {
        assert_eq!(r.get(i), *want, "column {i}");
    }
    // Absent and present-but-empty are different, which is what the
    // bitmap buys over an offset-equality convention.
    assert!(!r.has(1));
    assert!(r.has(2));
    assert_eq!(r.len(), 3);
}

#[test]
fn the_cost_scales_with_the_row_rather_than_sitting_on_a_floor() {
    // The defect being removed: a fixed cost independent of shape.
    let three = PackedRow::build(&names(3), &[Some(&b"x"[..]); 3]).expect("fits");
    let twelve = PackedRow::build(&names(12), &[Some(&b"x"[..]); 12]).expect("fits");
    assert!(
        twelve.heap_bytes() > three.heap_bytes(),
        "a wider row must cost more, not the same: {} vs {}",
        three.heap_bytes(),
        twelve.heap_bytes()
    );
    // Payload buffer plus the boxed inner, and nothing else. The inner is
    // the price of `Value`'s 32-byte cap; it is a constant, so it does not
    // reintroduce the floor — it shifts the line the row scales from.
    let inner = core::mem::size_of::<PackedInner>();
    assert_eq!(three.heap_bytes(), (2 + 1 + 3 * 2 + 3) + inner);
    assert_eq!(twelve.heap_bytes(), (2 + 2 + 12 * 2 + 12) + inner);
}

#[test]
fn replacing_a_column_leaves_the_others_alone() {
    let r = PackedRow::build(&names(3), &[Some(&b"a"[..]), Some(&b"bb"[..]), Some(&b"ccc"[..])])
        .expect("fits");
    let r2 = r.with_column(1, Some(b"REPLACED")).expect("fits");
    assert_eq!(r2.get(0), Some(&b"a"[..]));
    assert_eq!(r2.get(1), Some(&b"REPLACED"[..]));
    assert_eq!(r2.get(2), Some(&b"ccc"[..]));
    let r3 = r.with_column(0, None).expect("fits");
    assert!(!r3.has(0));
    assert_eq!(r3.get(2), Some(&b"ccc"[..]));
}

#[test]
fn an_in_place_write_touches_exactly_one_column() {
    // The failure this guards is not "the write did not happen" — it is a
    // write that lands one column over. Every neighbour is checked.
    let n = names(4);
    let mut r = PackedRow::build(
        &n,
        &[Some(&b"aaa"[..]), Some(&b"bbb"[..]), Some(&b"ccc"[..]), Some(&b"ddd"[..])],
    )
    .expect("fits");
    assert!(r.set_same_width(1, b"XXX"), "same width goes in place");
    assert_eq!(r.get(0), Some(&b"aaa"[..]), "left neighbour untouched");
    assert_eq!(r.get(1), Some(&b"XXX"[..]));
    assert_eq!(r.get(2), Some(&b"ccc"[..]), "right neighbour untouched");
    assert_eq!(r.get(3), Some(&b"ddd"[..]));
    // The first and last columns are the ones an off-by-one reaches past.
    assert!(r.set_same_width(0, b"ZZZ"));
    assert_eq!(r.get(0), Some(&b"ZZZ"[..]));
    assert_eq!(r.get(1), Some(&b"XXX"[..]));
    assert!(r.set_same_width(3, b"WWW"));
    assert_eq!(r.get(2), Some(&b"ccc"[..]));
    assert_eq!(r.get(3), Some(&b"WWW"[..]));
}

#[test]
fn an_in_place_write_refuses_anything_that_would_move_an_offset() {
    let n = names(3);
    let mut r = PackedRow::build(&n, &[Some(&b"aa"[..]), None, Some(&b"cc"[..])]).expect("fits");
    assert!(!r.set_same_width(0, b"aaa"), "wider must rebuild");
    assert!(!r.set_same_width(0, b"a"), "narrower must rebuild");
    assert!(!r.set_same_width(1, b"xx"), "an absent column must rebuild");
    assert!(!r.set_same_width(9, b"xx"), "out of range");
    // And a refusal changes nothing.
    assert_eq!(r.get(0), Some(&b"aa"[..]));
    assert_eq!(r.get(2), Some(&b"cc"[..]));
    assert!(!r.has(1));
}

#[test]
fn looks_a_column_up_by_the_name_the_wire_uses() {
    let n: ColumnNames = vec![b"id".to_vec(), b"name".to_vec(), b"dept".to_vec()].into();
    let r = PackedRow::build(&n, &[Some(&b"7"[..]), None, Some(&b"eng"[..])]).expect("fits");
    assert_eq!(r.get_named(b"id"), Some(&b"7"[..]));
    assert_eq!(r.get_named(b"dept"), Some(&b"eng"[..]));
    // Declared but absent on this row, and undeclared, both read as None
    // — but only the first is a column of the table.
    assert_eq!(r.get_named(b"name"), None);
    assert!(!r.has_named(b"name"));
    assert_eq!(r.get_named(b"nosuch"), None);
    assert!(!r.has_named(b"nosuch"));
}

#[test]
fn refuses_a_payload_it_cannot_address() {
    let big = vec![0u8; PACKED_MAX + 1];
    assert!(PackedRow::build(&names(1), &[Some(&big[..])]).is_none());
    let just = vec![0u8; PACKED_MAX];
    assert!(PackedRow::build(&names(1), &[Some(&just[..])]).is_some());
}

/// The claim this type exists for, as arithmetic rather than prose.
///
/// Today a promoted hash costs a 16-slot table (16 × 48 slot bytes plus
/// 16 + 16 metadata = 800 B requested), an `ArcInner` plus the map struct
/// (72 B), and a separate chunk for any value past the inline threshold.
/// A packed row is one buffer.
#[test]
fn a_packed_row_costs_less_than_the_table_it_replaces() {
    const TABLE_REQUEST: usize = 16 * 48 + 16 + 16; // slots + metadata
    const ARC_AND_MAP: usize = 16 + 56;
    for (ncol, vlen) in [(3usize, 400usize), (7, 400), (12, 400)] {
        let v = vec![b'x'; vlen / ncol];
        let cols: Vec<Option<&[u8]>> = (0..ncol).map(|_| Some(&v[..])).collect();
        let packed = PackedRow::build(&names(ncol), &cols).expect("fits").heap_bytes();
        let today = TABLE_REQUEST + ARC_AND_MAP + vlen;
        assert!(
            packed * 2 < today,
            "{ncol} columns: packed {packed} B is not less than half of today's {today} B"
        );
    }
}

/// And — the point of the finding — the cost must MOVE with the shape.
#[test]
fn the_cost_is_not_flat_in_the_column_count() {
    let v = [b'x'; 32];
    let w = |n: usize| {
        PackedRow::build(&names(n), &(0..n).map(|_| Some(&v[..])).collect::<Vec<_>>())
            .expect("fits")
            .heap_bytes()
    };
    let (a, b) = (w(3), w(12));
    // Nine more columns of 32 bytes each, plus nine more ends.
    assert_eq!(b - a, 9 * (32 + 2) + 1, "growth is payload + ends + bitmap byte");
}

fn unix_ms() -> u64 {
    let d = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).expect("after 1970");
    u64::try_from(d.as_millis()).expect("fits")
}

fn names_of(ns: &[&str]) -> ColumnNames {
    ns.iter().map(|n| n.as_bytes().to_vec()).collect()
}

#[test]
fn a_field_past_its_deadline_is_dropped_before_the_row_is_packed() {
    let mut s = crate::Store::new();
    s.hset(b"row", &[(b"id".as_slice(), b"7".as_slice()), (b"tmp", b"x")]).unwrap();
    s.hexpire_at(b"row", &[b"tmp".as_slice()], unix_ms() + 1, crate::HExpireCond::Always).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(5));
    s.pack_row(b"row", &names_of(&["id"]));
    assert!(s.is_packed(b"row"), "only declared fields are left once the dead one is gone");
    assert_eq!(s.hlen(b"row").unwrap(), 1);
}

#[test]
fn with_field_deadlines_set_a_key_that_is_no_hash_is_left_alone() {
    let mut s = crate::Store::new();
    s.hset(b"h", &[(b"f".as_slice(), b"v".as_slice())]).unwrap();
    s.hexpire_at(b"h", &[b"f".as_slice()], unix_ms() + 60_000, crate::HExpireCond::Always).unwrap();
    s.set(b"str", b"v".to_vec(), None, crate::SetCondition::Always);
    s.pack_row(b"str", &names_of(&["f"]));
    s.pack_row(b"missing", &names_of(&["f"]));
    assert_eq!(s.get(b"str").unwrap().as_deref(), Some(&b"v"[..]));
    assert_eq!(s.exists(&[b"missing".as_slice()]), 0);
}

#[test]
fn a_sharded_hash_is_packed_only_when_the_table_declares_every_field() {
    let n = crate::seg_map::HS_PROMOTE + 1;
    let fields: Vec<Vec<u8>> = (0..n).map(|i| format!("f{i}").into_bytes()).collect();
    let pairs: Vec<(&[u8], &[u8])> = fields.iter().map(|f| (&f[..], &b"v"[..])).collect();
    let mut s = crate::Store::new();
    s.hset(b"wide", &pairs).unwrap();
    assert!(matches!(
        s.map.get(b"wide".as_slice()).map(|e| &e.value),
        Some(crate::Value::SegHash(_))
    ));
    let short: ColumnNames = fields[..n - 1].to_vec().into();
    s.pack_row(b"wide", &short);
    assert!(!s.is_packed(b"wide"), "more fields than columns");
    let mut undeclared = fields.clone();
    undeclared[0] = b"other".to_vec();
    s.pack_row(b"wide", &undeclared.into());
    assert!(!s.is_packed(b"wide"), "a field the table lacks would be lost");
    s.pack_row(b"wide", &fields.into());
    assert!(s.is_packed(b"wide"));
    assert_eq!(s.hlen(b"wide").unwrap(), n);
    assert_eq!(s.hget(b"wide", b"f7").unwrap(), Some(&b"v"[..]));
}
