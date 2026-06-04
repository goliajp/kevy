//! Integration tests for `Config::to_toml_string_preserving` — the
//! comment-preserving re-emit path used by `CONFIG REWRITE`. Kept in the
//! `tests/` dir so the line count is exempt from the project's 500-LOC
//! source-file cap; only the public API is exercised.

use kevy_config::{Config, ConfigError};

fn rewrite(src: &str) -> String {
    Config::default()
        .to_toml_string_preserving(src)
        .unwrap_or_else(|e| panic!("preserving rewrite failed: {e}"))
}

#[test]
fn full_template_round_trips_byte_identically() {
    let src = Config::default().to_toml_string();
    assert_eq!(rewrite(&src), src);
}

#[test]
fn comment_preserved_and_value_substituted() {
    let src = "# top comment\n\
               [server]\n\
               # before port\n\
               port = 6004 # inline\n\
               ";
    let mut cfg = Config::default();
    cfg.server.port = 7000;
    let out = cfg
        .to_toml_string_preserving(src)
        .expect("preserving rewrite");
    assert!(out.contains("# top comment\n"), "lost top comment: {out:?}");
    assert!(
        out.contains("# before port\n"),
        "lost above-line comment: {out:?}"
    );
    assert!(
        out.contains("port = 7000 # inline\n"),
        "value substitution / inline comment broken: {out:?}",
    );
}

#[test]
fn blank_lines_preserved() {
    let src = "\n\n[server]\n\nport = 6004\n\n";
    let out = rewrite(src);
    // Two leading blank lines + one between section and key.
    assert!(
        out.starts_with("\n\n[server]\n\n"),
        "leading blanks lost: {out:?}"
    );
}

#[test]
fn missing_sections_appended() {
    let src = "[server]\nport = 6004\n";
    let out = rewrite(src);
    assert!(out.starts_with("[server]\nport = 6004\n"));
    for section in [
        "[persistence]",
        "[memory]",
        "[expiry]",
        "[log]",
        "[notification]",
        "[advanced]",
        "[slowlog]",
    ] {
        assert!(out.contains(section), "missing {section} in {out:?}");
    }
    assert!(out.contains("threads = "));
    assert!(out.contains("data_dir = "));
}

#[test]
fn missing_keys_for_known_section_inline_after_last_pair() {
    // [server] is already in the source, so missing server keys should
    // stay with it (no extra section header / blank line) and the next
    // orphan section starts after a single blank line.
    let src = "[server]\nport = 6004\n";
    let out = rewrite(src);
    assert!(
        out.starts_with("[server]\nport = 6004\n"),
        "preserved prefix lost: {out:?}",
    );
    // Threads/data_dir/bind appended INLINE in [server] (no new header).
    assert!(
        out.contains("port = 6004\n") && out.contains("\nthreads = 0\n"),
        "expected inline append in [server]: {out:?}",
    );
    // No spurious second [server] header.
    assert_eq!(
        out.matches("[server]").count(),
        1,
        "saw duplicate [server] header: {out:?}",
    );
    // First orphan section is separated by exactly one blank line.
    assert!(
        out.contains("\n\n[persistence]\n"),
        "expected blank line before first orphan section: {out:?}",
    );
}

#[test]
fn inline_appended_keys_come_before_following_section_blank() {
    // Source has [server]/port + blank + [memory]/maxmemory. The missing
    // [server] keys must land BEFORE the blank line separating sections.
    let src = "[server]\nport = 6004\n\n[memory]\nmaxmemory = 0\n";
    let out = rewrite(src);
    // After port = 6004 → missing server keys (bind/threads/data_dir),
    // THEN the existing blank line, THEN [memory].
    assert!(
        out.contains("port = 6004\nbind = ") && out.contains("\ndata_dir = \".\"\n"),
        "missing inline append in [server]: {out:?}",
    );
    assert!(
        out.contains("\n\n[memory]\n"),
        "lost blank-line separator before [memory]: {out:?}",
    );
}

#[test]
fn inline_comment_after_section_header_preserved() {
    let src = "[server] # the server block\nport = 6004\n";
    let out = rewrite(src);
    assert!(
        out.contains("[server] # the server block\n"),
        "got: {out:?}"
    );
}

#[test]
fn single_quoted_value_replaced_with_double_quote_canonical() {
    let src = "[server]\nbind = '127.0.0.1'\n";
    let out = rewrite(src);
    assert!(out.contains("bind = \"127.0.0.1\"\n"), "got: {out:?}");
}

#[test]
fn unterminated_string_is_a_parse_error() {
    let src = "[server]\nbind = \"127.0.0.1\n";
    let err = Config::default()
        .to_toml_string_preserving(src)
        .expect_err("should reject");
    assert!(matches!(err, ConfigError::Parse { .. }));
}

#[test]
fn unknown_key_in_source_is_passed_through_verbatim() {
    // Config::load would reject this, but the file could be edited after
    // kevy loaded it. We pass it through rather than dropping it.
    let src = "[server]\nport = 6004\nzzz = 1\n";
    let out = rewrite(src);
    assert!(out.contains("\nzzz = 1\n"), "dropped unknown key: {out:?}");
}

#[test]
fn whitespace_alignment_preserved() {
    let src = "[server]\nport     = 6004\nthreads  = 4\n";
    let mut cfg = Config::default();
    cfg.server.threads = 4;
    let out = cfg
        .to_toml_string_preserving(src)
        .expect("preserving rewrite");
    assert!(
        out.contains("port     = 6004\n"),
        "alignment dropped: {out:?}"
    );
    assert!(out.contains("threads  = 4\n"), "alignment dropped: {out:?}");
}

#[test]
fn source_without_trailing_newline_round_trips() {
    let src = Config::default().to_toml_string();
    let no_nl = src.trim_end_matches('\n').to_string();
    assert_eq!(rewrite(&no_nl), no_nl);
}

#[test]
fn value_substitution_updates_only_value_bytes() {
    // Change a single field; everything else in the source must persist
    // byte-identically.
    let src = "# header\n\
               [server]\n\
               port     = 6004\n\
               threads  = 0\n\
               [memory]\n\
               maxmemory = 0\n\
               ";
    let mut cfg = Config::default();
    cfg.memory.maxmemory = 64 * 1024 * 1024;
    let out = cfg
        .to_toml_string_preserving(src)
        .expect("preserving rewrite");
    assert!(out.contains("# header\n"));
    assert!(out.contains("port     = 6004\n"), "got: {out:?}");
    assert!(out.contains("threads  = 0\n"), "got: {out:?}");
    assert!(
        out.contains(&format!("maxmemory = {}\n", 64 * 1024 * 1024)),
        "got: {out:?}",
    );
}
