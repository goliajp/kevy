//! Fuzz `kevy_resp::parse_command` on arbitrary input. The parser must:
//!   1. never panic
//!   2. never loop forever
//!   3. never read past the input bound
//!   4. return one of {Ok(Some(Command, consumed)) | Ok(None) | Err(ProtocolError)}
//!      — total function over `&[u8]`.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Just calling parse_command on the input is the assertion. libfuzzer
    // catches panics / OOM / hangs automatically.
    let _ = kevy_resp::parse_command(data);
});
