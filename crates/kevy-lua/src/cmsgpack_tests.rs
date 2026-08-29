//! Bounds on counts read out of a msgpack header — split out of
//! `cmsgpack.rs` to keep it under the 500-line house rule.
//!
//! Measured before the bound existed: five bytes declaring an array32 of
//! 100,000,000 reserved 1,525 MB, and `u32::MAX` reserves about 65 GB for
//! `Vec<Value>` — twice that for a map's `Vec<(Value, Value)>`. An
//! allocation refusal calls `handle_alloc_error`, which aborts the process
//! rather than failing the command. `cmsgpack.unpack` is a global installed
//! for every script, so the whole input is
//! `EVAL "return cmsgpack.unpack(ARGV[1])" 0 <five bytes>` from any client:
//! eighth site of this shape in the release, and the only one reachable
//! over the wire.

use super::elements_fit;

/// An element count out of a msgpack header cannot size an allocation.
///
/// Reachable from any client: `cmsgpack.unpack` is a global installed
/// for every script, so five bytes of ARGV carry the claim. Measured
/// before the bound: `0xdd` plus a count of 100,000,000 reserved
/// 1,525 MB from a five-byte input; at u32::MAX the request is large
/// enough that a refusal aborts the process rather than failing the
/// command.
#[test]
fn an_element_count_from_a_header_cannot_size_an_allocation() {
    assert_eq!(elements_fit(3, 1024, 1), 3, "an honest count is used as-is");
    assert_eq!(
        elements_fit(u32::MAX as usize, 0, 1),
        1,
        "a count with no bytes behind it reserves nothing worth having"
    );
    assert_eq!(
        elements_fit(100_000_000, 4, 1),
        5,
        "four bytes cannot supply a hundred million elements"
    );
    assert_eq!(
        elements_fit(100_000_000, 4, 2),
        3,
        "a map entry is a key and a value, so half as many again"
    );
    // One byte per element is the floor, so an honest input is never
    // short-reserved.
    for len in [0usize, 1, 64, 4096] {
        assert_eq!(elements_fit(len, len, 1), len, "len at {len} fits exactly");
    }
}

/// The decode still refuses, as it did before — which is why the
/// assertions above are on the bound and not on the result.
#[test]
fn a_lying_array_header_is_still_an_error() {
    use luna_core::version::LuaVersion;
    let mut p = vec![0xddu8];
    p.extend_from_slice(&u32::MAX.to_be_bytes());
    let mut vm = super::Vm::new(LuaVersion::Lua54);
    let mut cur = 0usize;
    assert!(super::decode_value(&mut vm, &p, &mut cur, 0).is_err());
}
