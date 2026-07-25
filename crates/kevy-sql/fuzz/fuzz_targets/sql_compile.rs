//! Parsers are the classic fuzz target: arbitrary bytes must never
//! panic the lexer / parser / compiler — every input is either a
//! `Compilation` or a positioned `SqlError`. On success, rendering
//! must not panic either, and a re-compile of the same input must be
//! deterministic.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(src) = std::str::from_utf8(data) else { return };
    match kevy_sql::compile(src) {
        Ok(c) => {
            let _ = c.render_script();
            let again = kevy_sql::compile(src).expect("deterministic success");
            assert_eq!(c, again, "compile must be deterministic");
        }
        Err(e) => {
            // Errors are anchored and displayable.
            assert!(e.line >= 1 && e.col >= 1);
            let _ = e.to_string();
        }
    }
});
