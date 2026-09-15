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

/// Every msgpack tag the decoder claims to handle, decoded and checked.
///
/// `cmsgpack.unpack` is a global installed for every script, so the tag
/// byte comes straight off the wire:
/// `EVAL "return cmsgpack.unpack(ARGV[1])" 0 <bytes>`. The dispatch is
/// thirty-odd arms and 146 of its regions had never executed — one test
/// covered `0xdd` with a lying length, and the rest of the wire vocabulary
/// was reachable only by a client sending it.
///
/// Scalars are checked by value, because that is where a wrong arm is
/// silent: `0xd1` read as unsigned instead of `i16` gives 65,535 where
/// −1 is meant, and both are integers. The sign-extension arms
/// (`0xd0`-`0xd3`) are here for exactly that.
#[test]
fn every_scalar_tag_decodes_to_its_value() {
    use luna_core::runtime::value::Value;
    use luna_core::version::LuaVersion;

    /// `Value` has no `PartialEq`, and a local expectation type is better
    /// than one anyway: it names the four shapes this table covers, so a
    /// row that decodes to a string or a table is a compile error rather
    /// than a comparison nobody wrote.
    #[derive(Debug)]
    enum Want {
        Int(i64),
        Float(f64),
        Bool(bool),
        Nil,
    }

    fn agrees(got: &Value, want: &Want) -> bool {
        match (got, want) {
            (Value::Int(a), Want::Int(b)) => a == b,
            (Value::Float(a), Want::Float(b)) => a == b,
            (Value::Bool(a), Want::Bool(b)) => a == b,
            (Value::Nil, Want::Nil) => true,
            _ => false,
        }
    }

    // (name, wire bytes, expected)
    let cases: &[(&str, &[u8], Want)] = &[
        ("positive fixint 0", &[0x00], Want::Int(0)),
        ("positive fixint 127", &[0x7f], Want::Int(127)),
        ("negative fixint -1", &[0xff], Want::Int(-1)),
        ("negative fixint -32", &[0xe0], Want::Int(-32)),
        ("nil", &[0xc0], Want::Nil),
        ("false", &[0xc2], Want::Bool(false)),
        ("true", &[0xc3], Want::Bool(true)),
        ("uint8", &[0xcc, 0xff], Want::Int(255)),
        ("uint16", &[0xcd, 0xff, 0xff], Want::Int(65_535)),
        ("uint32", &[0xce, 0xff, 0xff, 0xff, 0xff], Want::Int(4_294_967_295)),
        // u64 -> i64 by cast: > i64::MAX comes back negative, matching
        // Redis, since Lua 5.1 has no unsigned type.
        ("uint64 max", &[0xcf, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff], Want::Int(-1)),
        ("int8 -1", &[0xd0, 0xff], Want::Int(-1)),
        ("int16 -1", &[0xd1, 0xff, 0xff], Want::Int(-1)),
        ("int32 -1", &[0xd2, 0xff, 0xff, 0xff, 0xff], Want::Int(-1)),
        ("int64 -1", &[0xd3, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff], Want::Int(-1)),
        ("int8 127", &[0xd0, 0x7f], Want::Int(127)),
        ("int16 -32768", &[0xd1, 0x80, 0x00], Want::Int(-32_768)),
        ("float32 1.5", &[0xca, 0x3f, 0xc0, 0x00, 0x00], Want::Float(1.5)),
        ("float64 1.5", &[0xcb, 0x3f, 0xf8, 0, 0, 0, 0, 0, 0], Want::Float(1.5)),
    ];

    let mut vm = super::Vm::new(LuaVersion::Lua54);
    for (name, bytes, want) in cases {
        let mut cur = 0usize;
        let got = super::decode_value(&mut vm, bytes, &mut cur, 0)
            .unwrap_or_else(|e| panic!("{name}: {bytes:02x?} failed to decode: {e}"));
        assert!(agrees(&got, want), "{name}: {bytes:02x?} decoded to {got:?}, wanted {want:?}");
        assert_eq!(cur, bytes.len(), "{name}: consumed {cur} of {} bytes", bytes.len());
    }
    assert_eq!(cases.len(), 19, "the table shrank; a table-driven test that loses rows tests less");
}

/// Truncate any of them by one byte and the decoder must refuse.
///
/// This is the half a length check exists for, and it is where a missing
/// bound would read a byte past the input. Every prefix of every case,
/// which is what the sampled version of this kind of test keeps getting
/// wrong elsewhere in this repository.
#[test]
fn every_scalar_tag_refuses_a_short_read() {
    use luna_core::version::LuaVersion;

    let cases: &[&[u8]] = &[
        &[0xcc, 0xff],
        &[0xcd, 0xff, 0xff],
        &[0xce, 0xff, 0xff, 0xff, 0xff],
        &[0xcf, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff],
        &[0xd0, 0xff],
        &[0xd1, 0xff, 0xff],
        &[0xd2, 0xff, 0xff, 0xff, 0xff],
        &[0xd3, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff],
        &[0xca, 0x3f, 0xc0, 0x00, 0x00],
        &[0xcb, 0x3f, 0xf8, 0, 0, 0, 0, 0, 0],
        &[0xa1, b'x'],
        &[0xd9, 1, b'x'],
        &[0xc4, 1, b'x'],
    ];

    let mut vm = super::Vm::new(LuaVersion::Lua54);
    let mut checked = 0;
    for full in cases {
        for cut in 1..full.len() {
            let mut cur = 0usize;
            assert!(
                super::decode_value(&mut vm, &full[..cut], &mut cur, 0).is_err(),
                "{:02x?} truncated to {cut} of {} bytes decoded as if whole",
                full,
                full.len()
            );
            checked += 1;
        }
    }
    assert!(checked >= 40, "only {checked} truncations exercised");
}

/// The two arms that must refuse whatever follows them: the reserved tag,
/// and the ext types this decoder does not implement.
///
/// `0xc1` is reserved by the msgpack spec and has no meaning; `0xc7`-`0xd8`
/// are ext/fixext, which the decoder deliberately errors on rather than
/// returning nil, so unknown data surfaces instead of vanishing.
#[test]
fn reserved_and_unimplemented_tags_are_errors_not_nil() {
    use luna_core::version::LuaVersion;

    let mut vm = super::Vm::new(LuaVersion::Lua54);
    for tag in [0xc1u8, 0xc7, 0xc8, 0xc9, 0xd4, 0xd5, 0xd6, 0xd7, 0xd8] {
        let mut cur = 0usize;
        let bytes = [tag, 0, 0, 0, 0, 0, 0, 0, 0];
        assert!(
            super::decode_value(&mut vm, &bytes, &mut cur, 0).is_err(),
            "tag 0x{tag:02x} must be an error, not a silently-decoded value"
        );
    }
}
