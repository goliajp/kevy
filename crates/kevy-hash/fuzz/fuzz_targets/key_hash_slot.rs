//! Fuzz `kevy_hash::key_hash_slot` (CRC16 + `{hashtag}` extraction) on
//! arbitrary keys. The slot decides key placement for the single-node
//! cluster mode, so it must agree with what external cluster clients
//! compute. Invariants asserted across arbitrary inputs:
//!
//!   * never panics (hashtag extraction does raw byte scanning)
//!   * slot is always < 16384
//!   * hashtag metamorphic property: for a tag with no braces, wrapping it
//!     as `{tag}<suffix>` hashes exactly like the bare tag — the rule
//!     cluster clients rely on for multi-key colocation

#![no_main]

use kevy_hash::key_hash_slot;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let slot = key_hash_slot(data);
    assert!(slot < 16384, "slot out of range: {slot}");

    // Metamorphic check: use the input as a tag when it can't confuse the
    // brace scanner (non-empty, brace-free).
    if !data.is_empty() && !data.iter().any(|&b| b == b'{' || b == b'}') {
        let mut wrapped = Vec::with_capacity(data.len() + 8);
        wrapped.push(b'{');
        wrapped.extend_from_slice(data);
        wrapped.push(b'}');
        wrapped.extend_from_slice(b"suffix");
        assert_eq!(
            key_hash_slot(&wrapped),
            key_hash_slot(data),
            "hashtag wrapping changed the slot"
        );
    }
});
