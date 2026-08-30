//! A packed row must be indistinguishable from the general one on the wire.
//!
//! Every hash read path ends in a `_ => WrongType` catch-all, so a
//! representation its arms do not name is not a compile error — it is a
//! WRONGTYPE at runtime, or a silently empty answer. The compiler cannot
//! hold this. These do.

use kevy_store::packed_row::{ColumnNames, PackedRow};
use kevy_store::{Store, Value};

/// Two stores holding the same row, one packed and one general.
fn both() -> (Store, Store, [&'static [u8]; 3]) {
    let cols: [&[u8]; 3] = [b"id", b"name", b"dept"];
    let vals: [Option<&[u8]>; 3] = [Some(b"7"), None, Some(b"eng")];
    let n: ColumnNames = cols.iter().map(|c| c.to_vec()).collect();
    let mut packed = Store::new();
    packed.load_value(b"row:1", &Value::PackedRow(PackedRow::build(&n, &vals).unwrap()), None);
    let mut general = Store::new();
    for (c, v) in cols.iter().zip(vals.iter()) {
        if let Some(v) = v {
            general.hset(b"row:1", &[(c, v)]).unwrap();
        }
    }
    (packed, general, cols)
}

/// The per-field verbs must agree for every column, including one the
/// table declares that this row does not have.
///
/// Every read path ends in a `_ => WrongType` catch-all, so a
/// representation its arms do not name is not a compile error — it is a
/// WRONGTYPE at runtime, or a silently empty answer. The compiler cannot
/// hold this; these tests do.
#[test]
fn the_per_field_verbs_agree_with_the_general_hash() {
    let (mut p, mut g, cols) = both();
    assert_eq!(p.hlen(b"row:1").unwrap(), g.hlen(b"row:1").unwrap());
    for c in &cols {
        let name = String::from_utf8_lossy(c);
        assert_eq!(
            p.hget(b"row:1", c).unwrap().map(<[u8]>::to_vec),
            g.hget(b"row:1", c).unwrap().map(<[u8]>::to_vec),
            "HGET {name}"
        );
        assert_eq!(
            p.hexists(b"row:1", c).unwrap(),
            g.hexists(b"row:1", c).unwrap(),
            "HEXISTS {name}"
        );
    }
    assert_eq!(p.hmget(b"row:1", &cols).unwrap(), g.hmget(b"row:1", &cols).unwrap());
}

/// Writing a declared column keeps the packed form and the other columns.
#[test]
fn a_declared_column_write_stays_packed() {
    let (mut p, _, _) = both();
    p.hset(b"row:1", &[(b"id".as_slice(), b"9".as_slice())]).unwrap();
    assert_eq!(p.hget(b"row:1", b"id").unwrap(), Some(&b"9"[..]));
    assert_eq!(p.hget(b"row:1", b"dept").unwrap(), Some(&b"eng"[..]));
    assert!(p.is_packed(b"row:1"), "still packed");
    // A different width rebuilds and must not lose a neighbour either.
    p.hset(b"row:1", &[(b"id".as_slice(), b"1234567".as_slice())]).unwrap();
    assert_eq!(p.hget(b"row:1", b"id").unwrap(), Some(&b"1234567"[..]));
    assert_eq!(p.hget(b"row:1", b"dept").unwrap(), Some(&b"eng"[..]));
    assert!(p.is_packed(b"row:1"), "still packed");
}

/// A column the table never declared cannot live in a packed row, so the
/// row leaves the form — carrying every value it had.
#[test]
fn an_undeclared_column_write_leaves_the_form_without_losing_data() {
    let (mut p, _, _) = both();
    p.hset(b"row:1", &[(b"extra".as_slice(), b"v".as_slice())]).unwrap();
    assert_eq!(p.hget(b"row:1", b"extra").unwrap(), Some(&b"v"[..]));
    assert_eq!(p.hget(b"row:1", b"id").unwrap(), Some(&b"7"[..]), "declared column survived");
    assert_eq!(p.hget(b"row:1", b"dept").unwrap(), Some(&b"eng"[..]));
    assert!(!p.is_packed(b"row:1"), "left the form");
}

/// The whole-row verbs must agree as sets — the general hash promises no
/// order, and HGETALL is a flat field/value stream, so it is paired up
/// before sorting or a field could compare against another field's value.
#[test]
fn the_whole_row_verbs_agree_with_the_general_hash() {
    let (mut p, mut g, _) = both();
    let sorted = |mut v: Vec<Vec<u8>>| {
        v.sort();
        v
    };
    assert_eq!(sorted(p.hkeys(b"row:1").unwrap()), sorted(g.hkeys(b"row:1").unwrap()));
    assert_eq!(sorted(p.hvals(b"row:1").unwrap()), sorted(g.hvals(b"row:1").unwrap()));
    let paired = |v: Vec<Vec<u8>>| {
        let mut q: Vec<(Vec<u8>, Vec<u8>)> =
            v.chunks(2).map(|c| (c[0].clone(), c[1].clone())).collect();
        q.sort();
        q
    };
    assert_eq!(paired(p.hgetall(b"row:1").unwrap()), paired(g.hgetall(b"row:1").unwrap()));
}

/// The mutating verbs a catch-all had been answering for.
///
/// `HDEL` and `HINCRBYFLOAT` reach the value through paths that named
/// the general forms only, so a packed row got WRONGTYPE about a row it
/// holds — and the field-TTL reaper deletes through `HDEL` and drops the
/// result, so an expired field on a packed row stayed readable forever.
#[test]
fn deleting_a_column_agrees_with_the_general_hash() {
    let (mut p, mut g, _) = both();
    assert_eq!(p.hdel(b"row:1", &[b"id", b"absent"]).unwrap(), 1);
    assert_eq!(g.hdel(b"row:1", &[b"id", b"absent"]).unwrap(), 1);
    assert_eq!(p.hget(b"row:1", b"id").unwrap(), None);
    assert_eq!(p.hget(b"row:1", b"dept").unwrap(), Some(&b"eng"[..]));
    assert_eq!(p.hlen(b"row:1").unwrap(), g.hlen(b"row:1").unwrap());
}

#[test]
fn deleting_every_column_drops_the_key_like_the_general_hash() {
    let (mut p, mut g, _) = both();
    p.hdel(b"row:1", &[b"id", b"dept"]).unwrap();
    g.hdel(b"row:1", &[b"id", b"dept"]).unwrap();
    assert_eq!(p.hlen(b"row:1").unwrap(), 0);
    let k: [&[u8]; 1] = [b"row:1"];
    assert_eq!(p.exists(&k), g.exists(&k), "an emptied hash takes its key with it");
}

#[test]
fn the_read_modify_write_verbs_agree_with_the_general_hash() {
    let (mut p, mut g, _) = both();
    p.hset(b"row:1", &[(b"id".as_slice(), b"7".as_slice())]).unwrap();
    assert_eq!(p.hincrbyfloat(b"row:1", b"id", 0.5).unwrap(), 7.5);
    assert_eq!(g.hincrbyfloat(b"row:1", b"id", 0.5).unwrap(), 7.5);
    assert_eq!(p.hincrby(b"row:1", b"n", 3).unwrap(), 3);
    assert_eq!(g.hincrby(b"row:1", b"n", 3).unwrap(), 3);
    assert!(!p.hsetnx(b"row:1", b"dept", b"sales").unwrap(), "the column is present");
    assert!(p.hsetnx(b"row:1", b"name", b"alice").unwrap(), "declared but absent");
    assert_eq!(p.hget(b"row:1", b"name").unwrap(), Some(&b"alice"[..]));
    assert_eq!(p.hget(b"row:1", b"dept").unwrap(), Some(&b"eng"[..]));
}
