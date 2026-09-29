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
