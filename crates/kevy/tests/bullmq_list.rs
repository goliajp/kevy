//! v1.27.3: list ops needed by BullMQ — RPOPLPUSH, LMOVE, LPOS.

use kevy_resp::Argv;
use kevy_store::Store;

fn argv(parts: &[&[u8]]) -> Argv {
    let mut a = Argv::default();
    for p in parts {
        a.push(p);
    }
    a
}

fn rpush(store: &mut Store, key: &[u8], vals: &[&[u8]]) {
    let mut parts: Vec<&[u8]> = vec![b"RPUSH", key];
    parts.extend_from_slice(vals);
    kevy::dispatch(store, &argv(&parts));
}

// ---- RPOPLPUSH ----------------------------------------------------

#[test]
fn rpoplpush_moves_tail_of_src_to_head_of_dst() {
    let mut store = Store::new();
    rpush(&mut store, b"src", &[b"a", b"b", b"c"]);
    rpush(&mut store, b"dst", &[b"x"]);
    let r = kevy::dispatch(&mut store, &argv(&[b"RPOPLPUSH", b"src", b"dst"]));
    assert_eq!(r, b"$1\r\nc\r\n");
    // src is now [a, b], dst is now [c, x]
    let g = kevy::dispatch(&mut store, &argv(&[b"LRANGE", b"src", b"0", b"-1"]));
    assert_eq!(g, b"*2\r\n$1\r\na\r\n$1\r\nb\r\n");
    let g = kevy::dispatch(&mut store, &argv(&[b"LRANGE", b"dst", b"0", b"-1"]));
    assert_eq!(g, b"*2\r\n$1\r\nc\r\n$1\r\nx\r\n");
}

#[test]
fn rpoplpush_empty_src_returns_nil() {
    let mut store = Store::new();
    let r = kevy::dispatch(&mut store, &argv(&[b"RPOPLPUSH", b"absent", b"dst"]));
    assert_eq!(r, b"$-1\r\n");
}

#[test]
fn rpoplpush_same_key_rotates() {
    let mut store = Store::new();
    rpush(&mut store, b"l", &[b"a", b"b", b"c"]);
    let r = kevy::dispatch(&mut store, &argv(&[b"RPOPLPUSH", b"l", b"l"]));
    assert_eq!(r, b"$1\r\nc\r\n");
    // After rotation: [c, a, b]
    let g = kevy::dispatch(&mut store, &argv(&[b"LRANGE", b"l", b"0", b"-1"]));
    assert_eq!(g, b"*3\r\n$1\r\nc\r\n$1\r\na\r\n$1\r\nb\r\n");
}

#[test]
fn rpoplpush_dst_wrong_type_errors_without_consuming() {
    let mut store = Store::new();
    rpush(&mut store, b"src", &[b"a"]);
    kevy::dispatch(&mut store, &argv(&[b"SET", b"dst", b"str"]));
    let r = kevy::dispatch(&mut store, &argv(&[b"RPOPLPUSH", b"src", b"dst"]));
    assert!(r.starts_with(b"-WRONGTYPE "));
    // src must still hold "a" because the pop got rolled back.
    let len = kevy::dispatch(&mut store, &argv(&[b"LLEN", b"src"]));
    assert_eq!(len, b":1\r\n");
}

// ---- LMOVE --------------------------------------------------------

#[test]
fn lmove_left_to_right_moves_head_to_tail() {
    let mut store = Store::new();
    rpush(&mut store, b"src", &[b"a", b"b", b"c"]);
    rpush(&mut store, b"dst", &[b"x"]);
    let r = kevy::dispatch(
        &mut store,
        &argv(&[b"LMOVE", b"src", b"dst", b"LEFT", b"RIGHT"]),
    );
    assert_eq!(r, b"$1\r\na\r\n");
    // src = [b, c], dst = [x, a]
    let g = kevy::dispatch(&mut store, &argv(&[b"LRANGE", b"dst", b"0", b"-1"]));
    assert_eq!(g, b"*2\r\n$1\r\nx\r\n$1\r\na\r\n");
}

#[test]
fn lmove_right_to_left_matches_rpoplpush() {
    let mut store = Store::new();
    rpush(&mut store, b"src", &[b"a", b"b", b"c"]);
    let r = kevy::dispatch(
        &mut store,
        &argv(&[b"LMOVE", b"src", b"dst", b"RIGHT", b"LEFT"]),
    );
    assert_eq!(r, b"$1\r\nc\r\n");
    let g = kevy::dispatch(&mut store, &argv(&[b"LRANGE", b"dst", b"0", b"-1"]));
    assert_eq!(g, b"*1\r\n$1\r\nc\r\n");
}

#[test]
fn lmove_bad_direction_errors() {
    let mut store = Store::new();
    rpush(&mut store, b"src", &[b"a"]);
    let r = kevy::dispatch(
        &mut store,
        &argv(&[b"LMOVE", b"src", b"dst", b"UP", b"LEFT"]),
    );
    assert!(r.starts_with(b"-ERR syntax error"));
}

#[test]
fn lmove_empty_src_returns_nil() {
    let mut store = Store::new();
    let r = kevy::dispatch(
        &mut store,
        &argv(&[b"LMOVE", b"absent", b"dst", b"LEFT", b"LEFT"]),
    );
    assert_eq!(r, b"$-1\r\n");
}

// ---- LPOS ---------------------------------------------------------

#[test]
fn lpos_default_returns_first_match_index() {
    let mut store = Store::new();
    rpush(&mut store, b"l", &[b"a", b"b", b"c", b"b", b"d"]);
    let r = kevy::dispatch(&mut store, &argv(&[b"LPOS", b"l", b"b"]));
    assert_eq!(r, b":1\r\n");
}

#[test]
fn lpos_absent_element_returns_nil_without_count() {
    let mut store = Store::new();
    rpush(&mut store, b"l", &[b"a", b"b"]);
    let r = kevy::dispatch(&mut store, &argv(&[b"LPOS", b"l", b"zzz"]));
    assert_eq!(r, b"$-1\r\n");
}

#[test]
fn lpos_count_returns_array_of_indices() {
    let mut store = Store::new();
    rpush(&mut store, b"l", &[b"a", b"b", b"c", b"b", b"d", b"b"]);
    // COUNT 0 = all matches
    let r = kevy::dispatch(&mut store, &argv(&[b"LPOS", b"l", b"b", b"COUNT", b"0"]));
    assert_eq!(r, b"*3\r\n:1\r\n:3\r\n:5\r\n");
}

#[test]
fn lpos_rank_negative_scans_from_tail() {
    let mut store = Store::new();
    rpush(&mut store, b"l", &[b"a", b"b", b"c", b"b", b"d"]);
    // RANK -1 = first match from tail; absolute index = 3
    let r = kevy::dispatch(&mut store, &argv(&[b"LPOS", b"l", b"b", b"RANK", b"-1"]));
    assert_eq!(r, b":3\r\n");
}

#[test]
fn lpos_rank_zero_errors() {
    let mut store = Store::new();
    rpush(&mut store, b"l", &[b"a"]);
    let r = kevy::dispatch(&mut store, &argv(&[b"LPOS", b"l", b"a", b"RANK", b"0"]));
    assert!(
        r.starts_with(b"-ERR RANK can't be zero"),
        "got {:?}",
        String::from_utf8_lossy(&r)
    );
}
