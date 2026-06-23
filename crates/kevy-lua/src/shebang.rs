//! Lua dialect / flags shebang parser.
//!
//! kevy-lua extends Redis 7.0's `#!lua name=...` Functions shebang
//! with a `version=` key that picks the luna dialect:
//!
//! ```lua
//! #!lua version=5.3
//! -- this script runs under Lua 5.3 (integer math, goto, bit ops)
//! local i = 10 // 3   -- 5.3+ integer divide
//! ```
//!
//! The parser is **only** consulted on the script's first line (up to
//! the first `\n`). Scripts without a shebang default to Lua 5.1 —
//! the Redis ecosystem compatibility anchor (see RFC L2).
//!
//! ## Grammar (intentionally permissive)
//!
//! ```text
//! shebang := '#!lua' ( whitespace key=val )* end-of-line
//! key=val := 'version=' (5.1 | 5.2 | 5.3 | 5.4 | 5.5)
//!          | 'flags='   any-non-whitespace      (parsed, ignored at v1.27)
//!          | 'name='    any-non-whitespace      (Redis 7.0 Functions; ignored for plain EVAL)
//! ```
//!
//! Unknown keys are tolerated (forward-compat with future Redis
//! Functions keys); an explicit unsupported `version=` (e.g. `5.6`)
//! is rejected.

use luna_core::version::LuaVersion;

/// Parsed shebang directives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Shebang {
    /// Selected dialect; defaults to 5.1.
    pub version: LuaVersion,
}

impl Default for Shebang {
    fn default() -> Self {
        Shebang {
            version: LuaVersion::Lua51,
        }
    }
}

/// Reason a shebang line was rejected. Returned to the bridge so it
/// can emit a wire-shaped `-ERR ...` reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ShebangError {
    /// `version=` had an unknown value (not 5.1/5.2/5.3/5.4/5.5).
    UnknownVersion(String),
}

impl std::fmt::Display for ShebangError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownVersion(v) => write!(f, "unknown lua version: {v}"),
        }
    }
}

/// Parse the script's first line. Returns:
///
/// - `Ok((Shebang, body))` — shebang found + the script body with
///   the shebang line stripped (so luna's parser doesn't have to
///   tolerate `#!lua` itself).
/// - `Ok((default, src))` — no shebang found; script passed through.
/// - `Err(ShebangError)` — shebang present but malformed.
pub(crate) fn parse(src: &[u8]) -> Result<(Shebang, &[u8]), ShebangError> {
    if !src.starts_with(b"#!") {
        return Ok((Shebang::default(), src));
    }
    // Find end of first line.
    let line_end = src.iter().position(|&b| b == b'\n').unwrap_or(src.len());
    let line = &src[..line_end];
    // The body starts after the `\n`. If we hit EOF without a `\n`
    // the body is empty.
    let body = if line_end < src.len() {
        &src[line_end + 1..]
    } else {
        &[]
    };

    // Body of the shebang after `#!`.
    let after_hashbang = &line[2..];
    // Expect optional whitespace then `lua`.
    let after_lua = match after_hashbang
        .strip_prefix(b"lua")
        .or_else(|| after_hashbang.trim_ascii_start().strip_prefix(b"lua"))
    {
        Some(s) => s,
        None => return Ok((Shebang::default(), src)), // `#!` but not `#!lua` → not for us
    };

    let mut shebang = Shebang::default();
    for kv in after_lua
        .split(|b| *b == b' ' || *b == b'\t')
        .filter(|s| !s.is_empty())
    {
        if let Some(rest) = kv.strip_prefix(b"version=") {
            shebang.version = parse_version(rest)?;
        } else if kv.starts_with(b"flags=") || kv.starts_with(b"name=") {
            // Recognized Redis Functions keys, intentionally ignored
            // for plain EVAL at v1.27. FUNCTION LOAD comes in v1.28+.
            continue;
        } else {
            // Unknown key — tolerate (forward compat). The Redis 7.0
            // Functions design is explicit that future versions may
            // add keys.
            continue;
        }
    }
    Ok((shebang, body))
}

fn parse_version(bytes: &[u8]) -> Result<LuaVersion, ShebangError> {
    match bytes {
        b"5.1" | b"51" => Ok(LuaVersion::Lua51),
        b"5.2" | b"52" => Ok(LuaVersion::Lua52),
        b"5.3" | b"53" => Ok(LuaVersion::Lua53),
        b"5.4" | b"54" => Ok(LuaVersion::Lua54),
        b"5.5" | b"55" => Ok(LuaVersion::Lua55),
        other => Err(ShebangError::UnknownVersion(
            String::from_utf8_lossy(other).into_owned(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_shebang_returns_default_with_src() {
        let (s, body) = parse(b"return 1").unwrap();
        assert_eq!(s.version, LuaVersion::Lua51);
        assert_eq!(body, b"return 1");
    }

    #[test]
    fn shebang_lua_51() {
        let (s, body) = parse(b"#!lua version=5.1\nreturn 1").unwrap();
        assert_eq!(s.version, LuaVersion::Lua51);
        assert_eq!(body, b"return 1");
    }

    #[test]
    fn shebang_lua_53_picks_53() {
        let (s, body) = parse(b"#!lua version=5.3\nreturn 1").unwrap();
        assert_eq!(s.version, LuaVersion::Lua53);
        assert_eq!(body, b"return 1");
    }

    #[test]
    fn shebang_lua_55_picks_55() {
        let (s, _) = parse(b"#!lua version=5.5\n").unwrap();
        assert_eq!(s.version, LuaVersion::Lua55);
    }

    #[test]
    fn shebang_numeric_form_accepted() {
        let (s, _) = parse(b"#!lua version=53\n").unwrap();
        assert_eq!(s.version, LuaVersion::Lua53);
    }

    #[test]
    fn shebang_with_extra_keys_tolerated() {
        let (s, body) =
            parse(b"#!lua version=5.3 flags=no-writes name=mylib\nreturn 1").unwrap();
        assert_eq!(s.version, LuaVersion::Lua53);
        assert_eq!(body, b"return 1");
    }

    #[test]
    fn shebang_unknown_key_tolerated_forward_compat() {
        let (s, body) = parse(b"#!lua version=5.4 future_key=value\nreturn 1").unwrap();
        assert_eq!(s.version, LuaVersion::Lua54);
        assert_eq!(body, b"return 1");
    }

    #[test]
    fn shebang_unknown_version_rejected() {
        let err = parse(b"#!lua version=5.6\nreturn 1").unwrap_err();
        assert!(matches!(err, ShebangError::UnknownVersion(ref v) if v == "5.6"));
    }

    #[test]
    fn shebang_without_lua_marker_passes_through() {
        // `#!/usr/bin/env something` — not ours. Treat the whole
        // script as 5.1 default; luna will parse the `#!` as a
        // comment-ish or error out.
        let (s, body) = parse(b"#!/foo\nreturn 1").unwrap();
        assert_eq!(s.version, LuaVersion::Lua51);
        assert_eq!(body, b"#!/foo\nreturn 1");
    }

    #[test]
    fn shebang_with_eof_no_newline() {
        // `#!lua version=5.3` exactly at EOF; body is empty.
        let (s, body) = parse(b"#!lua version=5.3").unwrap();
        assert_eq!(s.version, LuaVersion::Lua53);
        assert_eq!(body, b"");
    }
}
