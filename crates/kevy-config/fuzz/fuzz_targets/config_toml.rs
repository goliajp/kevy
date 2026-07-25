//! Fuzz `kevy_config::Config::from_toml_str` on arbitrary text.
//!
//! The config file is a trust boundary — arbitrary bytes on disk reach
//! this parser at server startup — so this is the security-highest
//! fuzz target in the workspace. Invariants asserted:
//!
//!   * the parser never panics on any input (the input is lossy-decoded
//!     to UTF-8 because `from_toml_str` takes `&str`; the lossy pass also
//!     exercises replacement-char handling in the lexer)
//!   * every parse/schema error carries a 1-based position (line ≥ 1,
//!     and col ≥ 1 for `Parse`) — the errors-carry-line-numbers
//!     contract
//!   * `from_toml_str` never returns `IoOpen` (it does no file I/O)
//!   * accepted inputs round-trip: `to_toml_string()` output must reparse
//!     successfully; for ASCII output (FINDING #3) it must re-serialize
//!     to the identical string (fixpoint) AND whole-Config equality holds
//!   * `parse_size` (the other text-facing entry: size literals from TOML
//!     and `CONFIG SET`) never panics on arbitrary text
//!
//! FIXED FINDING #1 (fuzz-found, minimized to the 1-byte
//! input `e`): every "unexpected end of input" error used to report
//! `line: 0, col: 0`. The parser now anchors EOF errors at the lexer's
//! end-of-input position, so the position assertion below covers the
//! EOF class too.
//!
//! FIXED FINDING #2 (found while writing this target):
//! `to_toml_string` emitted only 8 of the 14 schema sections —
//! [cluster]/[replication]/[lua]/[metrics]/[audit]/[feed] (and
//! `server.max_clients`) were silently dropped by `CONFIG REWRITE`.
//! Both serializers now share one canonical pair list; whole-Config
//! round-trip equality is asserted unconditionally (for ASCII output,
//! see FINDING #3).
//!
//! KNOWN FINDING #3 (fuzz-found, verified
//! with the 1-line repro `data_dir = "/é"` → parses as `"/Ã©"`): the
//! lexer accumulates quoted-string values byte-by-byte with `b as char`
//! (in `lex.rs`, both the quoted-string and unquoted-token paths), so every
//! non-ASCII byte is reinterpreted as its Latin-1 code point — any
//! UTF-8 path or value is corrupted on the FIRST parse, and each
//! parse→serialize cycle adds another mojibake layer (the fixpoint
//! assertion caught the second layer). Reported upstream, not fixed
//! here; fixpoint and round-trip equality are asserted only for ASCII
//! serializations until the lexer decodes UTF-8.
//!
//! KNOWN FINDING #4 (fuzz-found, verified with the 1-line
//! repro `data_dir = "a\nb"`): the lexer DECODES the escapes \n \t \r
//! \" \\ into the value, but `escape_toml_basic_string` re-escapes only
//! `\` and `"` — so a value holding a control character serializes raw
//! and the output no longer parses ("newline inside string" for \n; a
//! literal TAB/CR lands in the emitted file for \t/\r). A `CONFIG
//! REWRITE` of such a config writes a file the server can't load back.
//! Reported upstream, not fixed here; reparse failure is tolerated
//! exactly when the serialization carries an embedded control
//! character, and panics for every other input.

#![no_main]

use kevy_config::{Config, ConfigError};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let text = String::from_utf8_lossy(data);
    match Config::from_toml_str(&text, None) {
        Ok(cfg) => {
            let ser = cfg.to_toml_string();
            let re = match Config::from_toml_str(&ser, None) {
                Ok(re) => re,
                Err(e) => {
                    // KNOWN FINDING #4: values holding control characters
                    // (decoded from \n/\t/\r escapes) serialize raw and
                    // break the reparse. Tolerated for exactly that class.
                    let embedded_control =
                        ser.chars().any(|c| c.is_control() && c != '\n')
                            || e.to_string().contains("newline inside string");
                    assert!(
                        embedded_control,
                        "serialized config failed to reparse: {e}\n--- serialized ---\n{ser}"
                    );
                    return;
                }
            };
            // KNOWN FINDING #3: non-ASCII values mojibake on every parse,
            // so the fixpoint/equality checks only run on ASCII output.
            if ser.is_ascii() {
                assert_eq!(
                    re.to_toml_string(),
                    ser,
                    "serialize -> parse -> serialize is not a fixpoint"
                );
                // Both sides parsed with source_path = None, so plain
                // equality is the round-trip semantic check.
                assert_eq!(re, cfg, "round-trip changed config semantics");
            }
        }
        Err(ConfigError::Parse { line, col, msg }) => {
            assert!(
                line >= 1 && col >= 1,
                "parse error missing position: {line}:{col} ({msg})"
            );
        }
        Err(ConfigError::Schema { line, .. }) => {
            assert!(line >= 1, "schema error missing line: {line}");
        }
        Err(e @ ConfigError::IoOpen { .. }) => {
            panic!("from_toml_str does no file I/O but returned {e}");
        }
    }
    let _ = kevy_config::parse_size(&text);
});
