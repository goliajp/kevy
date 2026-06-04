//! Comment-preserving TOML re-emit for `CONFIG REWRITE`.
//!
//! [`Config::to_toml_string`] renders the live config via a fixed template —
//! every comment and custom ordering the user had in their hand-edited
//! `kevy.toml` is lost. This module adds a line-oriented re-parse of the
//! original source that records, per `key = value` line, the byte span of
//! the value substring. When re-emitting, the value bytes are spliced with
//! the live config's canonical formatting and every other byte
//! (indentation, alignment whitespace, inline `# comment` tails, full-line
//! comments, blank lines, section headers) flows through verbatim.
//!
//! Schema fields not present in the source are appended at file end,
//! grouped by section. The handler in `kevy` calls
//! [`Config::to_toml_string_preserving`] first, then falls back to the
//! template re-emit if re-parsing fails (e.g. the file was hand-mutated
//! after kevy loaded it).

use crate::schema::{Config, ConfigError, LogOutput};

/// Line-by-line view of an original `kevy.toml` source preserving every
/// byte the user wrote. Built by [`Document::parse`]; consumed by
/// [`Config::to_toml_string_preserving`].
pub(crate) struct Document {
    lines: Vec<Line>,
    /// True iff the original source ended with a `\n` byte. We restore
    /// that exact shape on emit so the rewrite is byte-identical when
    /// nothing changes.
    trailing_newline: bool,
}

struct Line {
    raw: String,
    kind: LineKind,
}

enum LineKind {
    BlankOrComment,
    Section(String),
    Pair {
        section: Option<String>,
        key: String,
        value_start: usize,
        value_end: usize,
    },
}

impl Document {
    pub(crate) fn parse(src: &str) -> Result<Self, ConfigError> {
        let trailing_newline = src.ends_with('\n');
        let mut lines = Vec::new();
        let mut current: Option<String> = None;
        for (idx, raw_with_nl) in src.split_inclusive('\n').enumerate() {
            let line_no = idx + 1;
            let raw = raw_with_nl.strip_suffix('\n').unwrap_or(raw_with_nl);
            let kind = classify_line(raw, &current, line_no)?;
            if let LineKind::Section(name) = &kind {
                current = Some(name.clone());
            }
            lines.push(Line { raw: raw.to_string(), kind });
        }
        Ok(Document { lines, trailing_newline })
    }
}

impl Config {
    /// Render the live config back into TOML preserving every comment,
    /// blank line, and key order from `original_source`. Schema fields
    /// missing from the source are appended at file end, grouped by
    /// section. Returns a [`ConfigError::Parse`] if `original_source`
    /// can't be re-parsed line-by-line (caller is expected to fall back
    /// to [`Self::to_toml_string`]).
    pub fn to_toml_string_preserving(
        &self,
        original_source: &str,
    ) -> Result<String, ConfigError> {
        let doc = Document::parse(original_source)?;
        let pairs = canonical_pairs(self);
        let last_idx = last_line_per_known_section(&doc);
        let mut emitted = vec![false; pairs.len()];
        let mut out = String::with_capacity(original_source.len() + 256);
        for (i, line) in doc.lines.iter().enumerate() {
            emit_line(line, &pairs, &mut emitted, &mut out);
            for (section_name, idx) in &last_idx {
                if *idx == i {
                    inline_flush_section(section_name, &pairs, &mut emitted, &mut out);
                }
            }
        }
        append_orphan_sections(&pairs, &mut emitted, &mut out, doc.trailing_newline);
        Ok(out)
    }
}

/// For each section that appears in `doc` (either as a `[name]` header or
/// as the implicit section of a pair line), the index of the LAST line in
/// `doc.lines` that belongs to it. New keys for that section are inlined
/// right after that line so they stay with the rest of the section.
fn last_line_per_known_section(doc: &Document) -> Vec<(String, usize)> {
    let mut acc: Vec<(String, usize)> = Vec::new();
    for (i, line) in doc.lines.iter().enumerate() {
        let name = match &line.kind {
            LineKind::Section(n) => Some(n.clone()),
            LineKind::Pair { section: Some(s), .. } => Some(s.clone()),
            _ => None,
        };
        if let Some(n) = name {
            if let Some(slot) = acc.iter_mut().find(|(k, _)| *k == n) {
                slot.1 = i;
            } else {
                acc.push((n, i));
            }
        }
    }
    acc
}

fn inline_flush_section(
    section: &str,
    pairs: &[CanonicalPair],
    emitted: &mut [bool],
    out: &mut String,
) {
    for (j, p) in pairs.iter().enumerate() {
        if !emitted[j] && p.section == section {
            out.push_str(p.key);
            out.push_str(" = ");
            out.push_str(&p.value);
            out.push('\n');
            emitted[j] = true;
        }
    }
}

fn append_orphan_sections(
    pairs: &[CanonicalPair],
    emitted: &mut [bool],
    out: &mut String,
    src_had_trailing_newline: bool,
) {
    let mut any_appended = false;
    let mut current_section: Option<&'static str> = None;
    for (i, p) in pairs.iter().enumerate() {
        if emitted[i] {
            continue;
        }
        if !any_appended {
            if !out.is_empty() {
                if !out.ends_with('\n') {
                    out.push('\n');
                }
                if !out.ends_with("\n\n") {
                    out.push('\n');
                }
            }
            any_appended = true;
        }
        if current_section != Some(p.section) {
            if current_section.is_some() {
                out.push('\n');
            }
            out.push('[');
            out.push_str(p.section);
            out.push_str("]\n");
            current_section = Some(p.section);
        }
        out.push_str(p.key);
        out.push_str(" = ");
        out.push_str(&p.value);
        out.push('\n');
        emitted[i] = true;
    }
    if !any_appended && !src_had_trailing_newline && out.ends_with('\n') {
        out.pop();
    }
}

fn emit_line(
    line: &Line,
    pairs: &[CanonicalPair],
    emitted: &mut [bool],
    out: &mut String,
) {
    match &line.kind {
        LineKind::BlankOrComment | LineKind::Section(_) => {
            out.push_str(&line.raw);
            out.push('\n');
        }
        LineKind::Pair { section, key, value_start, value_end } => {
            let canonical = pairs.iter().enumerate().find(|(_, p)| {
                p.section == section.as_deref().unwrap_or("") && p.key == key
            });
            match canonical {
                Some((idx, p)) => {
                    out.push_str(&line.raw[..*value_start]);
                    out.push_str(&p.value);
                    out.push_str(&line.raw[*value_end..]);
                    out.push('\n');
                    emitted[idx] = true;
                }
                None => {
                    // Unknown (section, key) — Config::load would have
                    // rejected it, so this branch is reachable only if
                    // the file was edited after kevy loaded it. Pass
                    // through verbatim rather than dropping it.
                    out.push_str(&line.raw);
                    out.push('\n');
                }
            }
        }
    }
}

// ───────────── canonical schema → TOML pairs ─────────────

struct CanonicalPair {
    section: &'static str,
    key: &'static str,
    value: String,
}

fn canonical_pairs(cfg: &Config) -> Vec<CanonicalPair> {
    let mut v = Vec::with_capacity(22);
    push_server(&mut v, cfg);
    push_persistence(&mut v, cfg);
    push_memory(&mut v, cfg);
    push_expiry(&mut v, cfg);
    push_log(&mut v, cfg);
    push_notification(&mut v, cfg);
    push_advanced(&mut v, cfg);
    push_slowlog(&mut v, cfg);
    v
}

fn push_server(v: &mut Vec<CanonicalPair>, cfg: &Config) {
    let [a, b, c, d] = cfg.server.bind;
    push(v, "server", "bind", format!("\"{a}.{b}.{c}.{d}\""));
    push(v, "server", "port", cfg.server.port.to_string());
    push(v, "server", "threads", cfg.server.threads.to_string());
    push(
        v,
        "server",
        "data_dir",
        toml_string(&cfg.server.data_dir.display().to_string()),
    );
}

fn push_persistence(v: &mut Vec<CanonicalPair>, cfg: &Config) {
    let p = &cfg.persistence;
    push(v, "persistence", "aof", p.aof.to_string());
    push(v, "persistence", "appendfsync", toml_string(p.appendfsync.as_str()));
    push(
        v,
        "persistence",
        "auto_aof_rewrite_percentage",
        p.auto_aof_rewrite_percentage.to_string(),
    );
    push(
        v,
        "persistence",
        "auto_aof_rewrite_min_size",
        p.auto_aof_rewrite_min_size.to_string(),
    );
}

fn push_memory(v: &mut Vec<CanonicalPair>, cfg: &Config) {
    push(v, "memory", "maxmemory", cfg.memory.maxmemory.to_string());
    push(
        v,
        "memory",
        "maxmemory_policy",
        toml_string(cfg.memory.maxmemory_policy.as_str()),
    );
}

fn push_expiry(v: &mut Vec<CanonicalPair>, cfg: &Config) {
    push(v, "expiry", "hz", cfg.expiry.hz.to_string());
    push(v, "expiry", "sample", cfg.expiry.sample.to_string());
}

fn push_log(v: &mut Vec<CanonicalPair>, cfg: &Config) {
    push(v, "log", "level", toml_string(cfg.log.level.as_str()));
    push(v, "log", "output", toml_string(&log_output_str(&cfg.log.output)));
}

fn push_notification(v: &mut Vec<CanonicalPair>, cfg: &Config) {
    push(
        v,
        "notification",
        "notify_keyspace_events",
        toml_string(&cfg.notification.notify_keyspace_events),
    );
}

fn push_advanced(v: &mut Vec<CanonicalPair>, cfg: &Config) {
    let a = &cfg.advanced;
    push(v, "advanced", "spin_limit", a.spin_limit.to_string());
    push(v, "advanced", "park_timeout_ms", a.park_timeout_ms.to_string());
    push(v, "advanced", "tick_check_every", a.tick_check_every.to_string());
    push(v, "advanced", "ring_capacity", a.ring_capacity.to_string());
}

fn push_slowlog(v: &mut Vec<CanonicalPair>, cfg: &Config) {
    push(
        v,
        "slowlog",
        "slower_than_micros",
        cfg.slowlog.slower_than_micros.to_string(),
    );
    push(v, "slowlog", "max_len", cfg.slowlog.max_len.to_string());
}

fn push(v: &mut Vec<CanonicalPair>, section: &'static str, key: &'static str, value: String) {
    v.push(CanonicalPair { section, key, value });
}

fn log_output_str(o: &LogOutput) -> String {
    o.as_str().into_owned()
}

fn toml_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

// ───────────── line classification ─────────────

fn classify_line(
    raw: &str,
    section_ctx: &Option<String>,
    line_no: usize,
) -> Result<LineKind, ConfigError> {
    let bytes = raw.as_bytes();
    let Some(i) = first_nonws(bytes) else {
        return Ok(LineKind::BlankOrComment);
    };
    let first = bytes[i];
    if first == b'#' {
        return Ok(LineKind::BlankOrComment);
    }
    if first == b'[' {
        return parse_section_line(bytes, i, line_no);
    }
    parse_pair_line(bytes, i, section_ctx, line_no)
}

fn parse_section_line(bytes: &[u8], i: usize, line_no: usize) -> Result<LineKind, ConfigError> {
    let rest = &bytes[i + 1..];
    let end = rest
        .iter()
        .position(|&b| b == b']')
        .ok_or_else(|| parse_err(line_no, i + 2, "expected ']' in section header"))?;
    let name = std::str::from_utf8(&rest[..end])
        .map_err(|_| parse_err(line_no, i + 2, "section name not UTF-8"))?
        .trim();
    if name.is_empty() {
        return Err(parse_err(line_no, i + 2, "empty section name"));
    }
    check_trailing_or_comment(&rest[end + 1..], line_no, i + end + 2)?;
    Ok(LineKind::Section(name.to_string()))
}

fn parse_pair_line(
    bytes: &[u8],
    key_start: usize,
    section_ctx: &Option<String>,
    line_no: usize,
) -> Result<LineKind, ConfigError> {
    let mut j = key_start;
    while j < bytes.len() && is_ident_char(bytes[j]) {
        j += 1;
    }
    if j == key_start {
        return Err(parse_err(line_no, key_start + 1, "expected key identifier"));
    }
    let key = std::str::from_utf8(&bytes[key_start..j])
        .map_err(|_| parse_err(line_no, key_start + 1, "key not UTF-8"))?
        .to_string();
    j = skip_ws(bytes, j);
    if j >= bytes.len() || bytes[j] != b'=' {
        return Err(parse_err(line_no, j + 1, "expected '='"));
    }
    j += 1;
    j = skip_ws(bytes, j);
    let value_start = j;
    let value_end = scan_value_end(bytes, j, line_no)?;
    check_trailing_or_comment(&bytes[value_end..], line_no, value_end + 1)?;
    Ok(LineKind::Pair {
        section: section_ctx.clone(),
        key,
        value_start,
        value_end,
    })
}

fn scan_value_end(bytes: &[u8], start: usize, line_no: usize) -> Result<usize, ConfigError> {
    if start >= bytes.len() {
        return Err(parse_err(line_no, start + 1, "expected value"));
    }
    let first = bytes[start];
    if first == b'"' || first == b'\'' {
        let mut k = start + 1;
        while k < bytes.len() {
            let b = bytes[k];
            if b == first {
                return Ok(k + 1);
            }
            if b == b'\\' && first == b'"' && k + 1 < bytes.len() {
                k += 2;
                continue;
            }
            k += 1;
        }
        return Err(parse_err(line_no, start + 1, "unterminated string"));
    }
    let mut k = start;
    while k < bytes.len() {
        let b = bytes[k];
        if b == b' ' || b == b'\t' || b == b'\r' || b == b'#' {
            break;
        }
        k += 1;
    }
    Ok(k)
}

fn check_trailing_or_comment(
    rest: &[u8],
    line_no: usize,
    col_base: usize,
) -> Result<(), ConfigError> {
    let mut k = 0;
    while k < rest.len() {
        let b = rest[k];
        if b == b' ' || b == b'\t' || b == b'\r' {
            k += 1;
            continue;
        }
        if b == b'#' {
            return Ok(());
        }
        return Err(parse_err(
            line_no,
            col_base + k,
            format!("unexpected trailing content {:?}", b as char),
        ));
    }
    Ok(())
}

fn first_nonws(bytes: &[u8]) -> Option<usize> {
    bytes
        .iter()
        .position(|&b| b != b' ' && b != b'\t' && b != b'\r')
}

fn skip_ws(bytes: &[u8], mut k: usize) -> usize {
    while k < bytes.len() && (bytes[k] == b' ' || bytes[k] == b'\t') {
        k += 1;
    }
    k
}

fn is_ident_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
}

fn parse_err(line: usize, col: usize, msg: impl Into<String>) -> ConfigError {
    ConfigError::Parse {
        line,
        col,
        msg: msg.into(),
    }
}

