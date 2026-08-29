//! A rank or index window entirely past the end is EMPTY, not the last
//! element.
//!
//! Redis floors a negative start at zero and caps only the end; a start
//! past the last index makes the window empty. `range_bounds` has had
//! that rule right all along — and `getrange` and `zrevrange` had each
//! written a clamp of their own beside it, both capping the start UP to
//! the last index. `GETRANGE k 99 200` on a 24-byte value answered the
//! last byte; `ZREVRANGE z 5 10` on three members answered a member.
//!
//! Both surfaces of this engine agreed with each other, which is why
//! the wire-versus-facade differential passed them and the three-way
//! against a real valkey 9.1 did not — 204 of 206, and these were the
//! two. The values below are valkey's.
//!
//! The tests are here, beside the one implementation both surfaces now
//! call, rather than in either of them.

use crate::Store;

fn store() -> Store {
    Store::new()
}

#[test]
fn getrange_past_the_end_is_empty() {
    let mut s = store();
    s.set_slice(b"k", b"hello-world", None, false, false);
    assert_eq!(s.getrange(b"k", 0, 4).unwrap(), b"hello".to_vec());
    assert_eq!(s.getrange(b"k", -5, -1).unwrap(), b"world".to_vec());
    // The whole window is past the last index: valkey answers "".
    assert_eq!(s.getrange(b"k", 99, 200).unwrap(), Vec::<u8>::new());
    assert_eq!(s.getrange(b"k", 11, 11).unwrap(), Vec::<u8>::new());
    // Start after stop, both in range: empty as well.
    assert_eq!(s.getrange(b"k", 3, 2).unwrap(), Vec::<u8>::new());
    // The end alone past the last index still returns what exists.
    assert_eq!(s.getrange(b"k", 6, 999).unwrap(), b"world".to_vec());
}

#[test]
fn zrevrange_past_the_end_is_empty() {
    let mut s = store();
    for (score, member) in [(1.0, &b"one"[..]), (2.0, b"two"), (3.0, b"three")] {
        s.zadd(b"z", &[(score, member)]).unwrap();
    }
    let members = |v: Vec<(Vec<u8>, f64)>| -> Vec<Vec<u8>> { v.into_iter().map(|(m, _)| m).collect() };

    assert_eq!(
        members(s.zrevrange(b"z", 0, -1).unwrap()),
        vec![b"three".to_vec(), b"two".to_vec(), b"one".to_vec()]
    );
    assert_eq!(members(s.zrevrange(b"z", 0, 0).unwrap()), vec![b"three".to_vec()]);
    assert_eq!(
        members(s.zrevrange(b"z", -2, -1).unwrap()),
        vec![b"two".to_vec(), b"one".to_vec()]
    );
    // Past the last rank: valkey answers an empty array.
    assert_eq!(members(s.zrevrange(b"z", 5, 10).unwrap()), Vec::<Vec<u8>>::new());
    assert_eq!(members(s.zrevrange(b"z", 3, 3).unwrap()), Vec::<Vec<u8>>::new());
    // Start after stop: empty.
    assert_eq!(members(s.zrevrange(b"z", 2, 1).unwrap()), Vec::<Vec<u8>>::new());
    // A missing key is empty, not an error.
    assert_eq!(members(s.zrevrange(b"nosuch", 0, -1).unwrap()), Vec::<Vec<u8>>::new());
}
