use super::*;
use crate::composite::{CompositeCol, composite_encode};

/// A codec with plain key suffixes, for keys that are not digits.
fn plain(form: Form, prefix: &[u8]) -> Codec {
    let mut c = Codec::new(form, prefix, 0);
    c.digits = false;
    c
}

fn e(c: &Codec, v: &IndexValue, k: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    c.put_entry(v, k, &mut out);
    out
}

fn roundtrip(c: &Codec, v: &IndexValue, k: &[u8]) {
    let enc = e(c, v, k);
    let vl = c.value_len(&enc);
    assert_eq!(&c.value(&enc[..vl]), v, "value of {v:?}");
    let mut key = Vec::new();
    c.key_into(&enc[vl..], &mut key);
    assert_eq!(key, k, "key of {v:?}");
}

#[test]
fn byte_order_is_value_then_key_order() {
    let ints = Codec::new(Form::I64, b"row:", 0);
    let strs = plain(Form::Str, b"");
    let mut pairs: Vec<(IndexValue, Vec<u8>)> = Vec::new();
    for v in [i64::MIN, -5, -1, 0, 1, 7, i64::MAX] {
        for k in ["row:", "row:0", "row:1", "row:10", "row:100", "row:2", "row:9", "row:09"] {
            pairs.push((IndexValue::I64(v), k.as_bytes().to_vec()));
        }
    }
    let mut by_bytes = pairs.clone();
    by_bytes.sort_by_key(|(v, k)| e(&ints, v, k));
    pairs.sort();
    assert_eq!(by_bytes, pairs);

    let mut sp: Vec<(IndexValue, Vec<u8>)> = Vec::new();
    for v in ["", "a", "a\0", "a\0b", "ab", "b", "\0"] {
        for k in ["", "a", "zz", "\0"] {
            sp.push((IndexValue::Str(v.as_bytes().to_vec()), k.as_bytes().to_vec()));
        }
    }
    let mut by_bytes = sp.clone();
    by_bytes.sort_by_key(|(v, k)| e(&strs, v, k));
    sp.sort();
    assert_eq!(by_bytes, sp);
}

#[test]
fn f64_orders_like_total_cmp() {
    let c = plain(Form::F64, b"");
    let vals = [f64::NEG_INFINITY, -1e300, -1.5, -0.0, 0.0, 1e-300, 2.5, f64::INFINITY];
    for w in vals.windows(2) {
        assert!(e(&c, &IndexValue::F64(w[0]), b"") < e(&c, &IndexValue::F64(w[1]), b""));
    }
    for v in vals {
        roundtrip(&c, &IndexValue::F64(v), b"k");
    }
}

#[test]
fn every_form_round_trips() {
    let ints = Codec::new(Form::I64, b"row:", 0);
    for k in ["row:", "row:0", "row:0012", "row:12345678901234567890"] {
        roundtrip(&ints, &IndexValue::I64(-9), k.as_bytes());
    }
    let mut raw = ints.clone();
    raw.digits = false;
    roundtrip(&raw, &IndexValue::I64(3), b"row:alice");
    let strs = plain(Form::Str, b"");
    roundtrip(&strs, &IndexValue::Str(b"a\0\0b\xFF".to_vec()), b"\0\xFF");
    roundtrip(&strs, &IndexValue::Str(Vec::new()), b"");
}

#[test]
fn a_composite_value_is_stored_as_it_is() {
    let cols = [
        CompositeCol::new("s", ValType::Str),
        CompositeCol::new("t", ValType::I64).with_order(SortOrder::Desc),
        CompositeCol::new("d", ValType::Str).with_order(SortOrder::Desc),
    ];
    let form = Form::Composite(cols.iter().map(|c| (c.ty, c.order)).collect());
    let c = Codec::new(form, b"t:", 0);
    for (s, t, d) in [("s1", "5", "x"), ("", "-3", ""), ("a\0b", "0", "\0\0")] {
        let enc =
            composite_encode(&cols, &[Some(s.as_bytes()), Some(t.as_bytes()), Some(d.as_bytes())])
                .expect("coerces");
        let v = IndexValue::Str(enc.clone());
        assert_eq!(e(&c, &v, b"t:42"), [enc.as_slice(), &[0x53]].concat(), "no extra framing");
        roundtrip(&c, &v, b"t:42");
    }
}

#[test]
fn digit_packing_keeps_order_and_length() {
    let mut all: Vec<&str> =
        vec!["", "0", "00", "01", "1", "10", "100", "11", "19", "2", "9", "99", "990"];
    all.sort();
    let packed: Vec<Vec<u8>> = all
        .iter()
        .map(|d| {
            let mut out = Vec::new();
            pack_digits(d.as_bytes(), &mut out);
            out
        })
        .collect();
    for w in packed.windows(2) {
        assert!(w[0] < w[1], "{w:?}");
    }
    for (d, p) in all.iter().zip(&packed) {
        assert_eq!(p.len(), d.len().div_ceil(2));
        let mut back = Vec::new();
        unpack_digits(p, &mut back);
        assert_eq!(back, d.as_bytes());
    }
}

#[test]
fn widening_drops_what_a_key_breaks() {
    let c = Codec::new(Form::I64, b"u:", 0);
    assert!(c.fits_key(b"u:12") && !c.fits_key(b"u:ab") && !c.fits_key(b"v:1"));
    let w = c.widened_for(b"u:ab");
    assert_eq!((&*w.prefix, w.digits), (&b"u:"[..], false));
    let w = c.widened_for(b"v:1");
    assert_eq!((&*w.prefix, w.digits), (&b""[..], false), "v:1 is not all digits once u: is gone");
    let w = c.widened_for(b"77");
    assert_eq!((&*w.prefix, w.digits), (&b""[..], true), "a bare number keeps digit keys");
}

#[test]
fn columns_come_back_as_they_went_in() {
    let vals: [Option<&[u8]>; 6] =
        [Some(b"s1"), Some(b"12345678"), None, Some(b""), Some(b"007"), Some(&[0u8, 255, 7])];
    let mut p = Vec::new();
    for v in vals {
        put_column(v, &mut p);
    }
    assert_eq!(p.len(), 3 + 5 + 1 + 1 + 3 + 4, "digits are packed, short strings pay one byte");
    let mut buf = Vec::new();
    for (i, v) in vals.iter().enumerate() {
        assert_eq!(nth_column(&p, i).bytes(&mut buf), *v, "column {i}");
    }
    let mut big = Vec::new();
    put_varint(300, &mut big);
    let mut at = 0;
    assert_eq!((varint(&big, &mut at), at), (300, 2));
}
