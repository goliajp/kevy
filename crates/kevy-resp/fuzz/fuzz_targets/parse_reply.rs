//! Fuzz `kevy_resp::parse_reply` on arbitrary input. Same contract as
//! parse_command: total function over `&[u8]`, no panics / hangs / OOB
//! reads. Reply variants include arrays which can nest arbitrarily — the
//! parser must bound recursion / iteration on adversarial input.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = kevy_resp::parse_reply(data);
});
