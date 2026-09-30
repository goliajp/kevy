//! The tagged binary form and the reply text of an [`IndexValue`].

use super::*;

#[test]
fn every_kind_of_value_renders_as_a_reply_shows_it() {
    assert_eq!(IndexValue::I64(-7).render(), b"-7");
    assert_eq!(IndexValue::F64(2.5).render(), b"2.5");
    assert_eq!(IndexValue::F64(-0.125).render(), b"-0.125");
    assert_eq!(IndexValue::Str(b"kyoto".to_vec()).render(), b"kyoto");
}

#[test]
fn a_run_of_encoded_values_decodes_back_in_order() {
    let vals = [
        IndexValue::I64(i64::MIN),
        IndexValue::F64(-1.5),
        IndexValue::Str(Vec::new()),
        IndexValue::Str(b"tokyo".to_vec()),
    ];
    let mut buf = Vec::new();
    for v in &vals {
        v.encode(&mut buf);
    }
    let mut pos = 0;
    for v in &vals {
        assert_eq!(IndexValue::decode(&buf, &mut pos).as_ref(), Some(v));
    }
    assert_eq!(pos, buf.len());
    assert_eq!(IndexValue::decode(&buf, &mut pos), None, "nothing past the end");
}

#[test]
fn a_truncated_or_unknown_value_decodes_to_nothing_and_leaves_pos() {
    let whole = |v: IndexValue| {
        let mut b = Vec::new();
        v.encode(&mut b);
        b
    };
    for full in [
        whole(IndexValue::I64(5)),
        whole(IndexValue::F64(0.5)),
        whole(IndexValue::Str(b"ab".into())),
    ] {
        for cut in 1..full.len() {
            let mut pos = 0;
            assert_eq!(IndexValue::decode(&full[..cut], &mut pos), None, "{full:?} cut at {cut}");
            assert_eq!(pos, 0);
        }
    }
    let mut pos = 0;
    assert_eq!(IndexValue::decode(&[3, 0, 0, 0, 0, 0, 0, 0, 0], &mut pos), None, "tag 3");
    assert_eq!(pos, 0);
}
