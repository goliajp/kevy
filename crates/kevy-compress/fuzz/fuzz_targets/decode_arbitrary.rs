//! K3's rejection half: arbitrary bytes fed to decode must either
//! error or produce a value — never panic, never overrun (bounds are
//! all checked; this target exists to let the fuzzer hunt for the
//! arithmetic case the checks missed).
#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Some((&split, rest)) = data.split_first() else { return };
    let cut = (split as usize * rest.len()) / 255;
    let (dict, frame) = rest.split_at(cut);
    let _ = kevy_compress::decode(dict, frame);
});
