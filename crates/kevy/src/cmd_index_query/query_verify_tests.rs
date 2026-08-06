//! `encode_verify_chunk`'s tests — both directions of the audit and the
//! windowed exemption. A `#[path]` child of `query`, split out for the
//! 500-LOC house rule.

use super::*;
use kevy_index::{IndexKind, ValType};

fn spec() -> IndexSpec {
    IndexSpec {
        name: b"byage".to_vec(),
        prefix: b"u:".to_vec(),
        fields: vec![kevy_index::FieldSpec::new(b"age".to_vec())],
        ty: ValType::I64,
        kind: IndexKind::Range,
        max_bytes: 0,
        ann: None,
        group_by: None,
        with_positions: false,
        values: Vec::new(),
        composite: None,
    }
}

fn stats() -> SegmentStats {
    SegmentStats { entries: 3, approx_bytes: 0, coerce_failures: 0, duplicates: 0 }
}

/// Read `drift` and `checked` back out of the wire chunk.
fn drift_and_checked(chunk: &[u8]) -> (u64, u64) {
    (field(chunk, 4), field(chunk, 5))
}

fn field(chunk: &[u8], i: usize) -> u64 {
    // Byte 0 is the status, byte 1 the kind tag ('s' here — these tests
    // build scalar segments); the u64 slots start at 2.
    assert_eq!(chunk[1], b's', "scalar chunks carry the scalar tag");
    u64::from_le_bytes(chunk[2 + i * 8..2 + (i + 1) * 8].try_into().expect("8 bytes"))
}

/// A drift counter that can only ever report zero is the dead code it
/// replaced. Diverge the store from the index behind the write hook's back
/// and prove all three shapes of disagreement are caught.
#[test]
fn drift_counts_every_way_an_entry_can_disagree_with_its_row() {
    let mut store = Store::new();
    // agrees
    store.hset(b"u:1", &[(b"age".as_slice(), b"30".as_slice())]).unwrap();
    // disagrees — the row says 41, the index holds 40
    store.hset(b"u:2", &[(b"age".as_slice(), b"41".as_slice())]).unwrap();
    // gone — no row at all, but the index still holds the key
    // (u:3 deliberately not written)

    let entries = vec![
        (b"u:1".to_vec(), IndexValue::I64(30)),
        (b"u:2".to_vec(), IndexValue::I64(40)),
        (b"u:3".to_vec(), IndexValue::I64(50)),
    ];
    let chunk = encode_verify_chunk(&mut store, &spec(), &entries, &stats(), None);
    let (drift, checked) = drift_and_checked(&chunk);
    assert_eq!(checked, 3, "every held entry must be re-read");
    assert_eq!(drift, 2, "the changed row and the missing row must both count");
}

#[test]
fn a_healthy_index_reports_zero_drift() {
    let mut store = Store::new();
    store.hset(b"u:1", &[(b"age".as_slice(), b"30".as_slice())]).unwrap();
    store.hset(b"u:2", &[(b"age".as_slice(), b"40".as_slice())]).unwrap();
    let entries = vec![
        (b"u:1".to_vec(), IndexValue::I64(30)),
        (b"u:2".to_vec(), IndexValue::I64(40)),
    ];
    let chunk = encode_verify_chunk(&mut store, &spec(), &entries, &stats(), None);
    assert_eq!(drift_and_checked(&chunk), (0, 2));
}

/// The direction a walk over the index's own entries cannot see: a
/// row that belongs in the index and has no entry. Nothing computed
/// it before, which is why `MOVE-SCOPE`-ingested rows could be
/// invisible to every query while VERIFY called the index clean.
#[test]
fn a_row_with_no_entry_counts_as_missing() {
    let mut store = Store::new();
    store.hset(b"u:1", &[(b"age".as_slice(), b"30".as_slice())]).unwrap();
    store.hset(b"u:2", &[(b"age".as_slice(), b"40".as_slice())]).unwrap();
    // u:2 exists and derives a value, but the index never heard of it.
    let entries = vec![(b"u:1".to_vec(), IndexValue::I64(30))];
    let chunk = encode_verify_chunk(&mut store, &spec(), &entries, &stats(), None);
    assert_eq!(drift_and_checked(&chunk), (0, 1), "the held entry is fine");
    assert_eq!(field(&chunk, 6), 1, "u:2 is a hole in the index");
}

/// A row the index does not owe an entry is not a hole: an
/// uncoercible field is `coerce_failures`, not `missing` (Law 3 —
/// absence is never an error).
#[test]
fn a_row_the_index_does_not_owe_is_not_missing() {
    let mut store = Store::new();
    store.hset(b"u:1", &[(b"age".as_slice(), b"nope".as_slice())]).unwrap();
    store.hset(b"u:2", &[(b"other".as_slice(), b"7".as_slice())]).unwrap();
    let chunk = encode_verify_chunk(&mut store, &spec(), &[], &stats(), None);
    assert_eq!(field(&chunk, 6), 0, "neither row derives a value");
}

fn audit(cold_live: u64) -> Option<kevy_index::WindowAudit> {
    Some(kevy_index::WindowAudit {
        boundary: 50,
        shape: kevy_index::WindowShape::PlainI64,
        cold_live,
    })
}

/// A windowed path slides old rows into cold segments on purpose, so
/// their absence from the hot entries is the design, not a hole —
/// **when the segment actually has them**. Without the boundary every
/// slid row is reported as a hole in the index that shed it.
#[test]
fn a_row_that_slid_out_of_the_window_is_not_missing() {
    let mut store = Store::new();
    store.hset(b"u:1", &[(b"age".as_slice(), b"10".as_slice())]).unwrap();
    store.hset(b"u:2", &[(b"age".as_slice(), b"90".as_slice())]).unwrap();
    let chunk = encode_verify_chunk(&mut store, &spec(), &[], &stats(), audit(1));
    assert_eq!(field(&chunk, 6), 1, "only the in-window row is a hole");
}

/// The reason the exemption cannot be positional: a row lost between
/// the hot tree and the segment sits below the boundary exactly like a
/// legitimate one. Same store, same boundary — only the cold side's
/// own count differs, and it is what tells the two apart.
#[test]
fn a_slid_row_the_cold_side_never_received_is_missing() {
    let mut store = Store::new();
    store.hset(b"u:1", &[(b"age".as_slice(), b"10".as_slice())]).unwrap();
    store.hset(b"u:2", &[(b"age".as_slice(), b"90".as_slice())]).unwrap();
    let chunk = encode_verify_chunk(&mut store, &spec(), &[], &stats(), audit(0));
    assert_eq!(field(&chunk, 6), 2, "the slid row never arrived: both are holes");
}

/// A cold entry can outlive its row — tombstoning is the writer's job.
/// That direction belongs to the drift walk, so a surplus must not
/// underflow into a negative hole count.
#[test]
fn more_cold_entries_than_slid_rows_is_not_a_hole() {
    let mut store = Store::new();
    store.hset(b"u:1", &[(b"age".as_slice(), b"10".as_slice())]).unwrap();
    let chunk = encode_verify_chunk(&mut store, &spec(), &[], &stats(), audit(9));
    assert_eq!(field(&chunk, 6), 0, "surplus cold entries are not holes");
}

/// A row whose field stopped coercing (someone wrote a string into an i64
/// index's field) is drift, not silence.
#[test]
fn a_row_that_no_longer_coerces_counts_as_drift() {
    let mut store = Store::new();
    store.hset(b"u:1", &[(b"age".as_slice(), b"not-a-number".as_slice())]).unwrap();
    let entries = vec![(b"u:1".to_vec(), IndexValue::I64(30))];
    let chunk = encode_verify_chunk(&mut store, &spec(), &entries, &stats(), None);
    assert_eq!(drift_and_checked(&chunk), (1, 1));
}
