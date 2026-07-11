//! Fuzz the replication mutation-frame decoder on arbitrary byte streams.
//!
//! This is the malicious-primary trust boundary: a replica decodes a
//! primary's stream with `decode_frame` directly off the socket. A
//! forged envelope, offset, or bulk length must surface as a
//! `WireError` or a "need more bytes" result — never a panic and never
//! an unbounded allocation. The decoder reconstructs the inner argv via
//! the (separately-fuzzed, `MAX_BULK_LEN`-capped) request parser, so
//! this target exercises the envelope + offset parsing plus that path.
//!
//! The asserted property: `decode_frame` terminates without panicking
//! across arbitrary inputs.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = kevy_replicate::wire::decode_frame(data);
});
