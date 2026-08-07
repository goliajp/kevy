//! K2 + K3 under fuzz: for ANY (dict, input) split, the frame must
//! round-trip to identity and must never exceed input + header.
#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Some((&split, rest)) = data.split_first() else { return };
    let cut = (split as usize * rest.len()) / 255;
    let (dict, input) = rest.split_at(cut);
    let frame = kevy_compress::encode(dict, input);
    assert!(frame.len() <= input.len() + 6, "K2 violated");
    let back = kevy_compress::decode(dict, &frame).expect("own frame decodes");
    assert_eq!(back, input, "round-trip identity");
});
