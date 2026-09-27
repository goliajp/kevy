//! `-u <uri>`: the URI forms redis-cli accepts, and what each part sets.
//!
//! Not `kevy_resp_client::parse_url`: that one is the kevy client's URL,
//! which refuses credentials because kevy has none. This is redis-cli's,
//! which carries a user and password to whatever server it names, keeps the
//! host already set when the URI has none, and has its own messages.

use super::cnum::atoi;
use super::opts::Opts;
use super::opts_parse::{Step, fail};

/// Apply `uri` to `o`; `Some(step)` when parsing ends the program.
pub(crate) fn apply_uri(o: &mut Opts, uri: &[u8]) -> Option<Step> {
    let lower = uri.to_ascii_lowercase();
    // DEV-006: kevy-cli has no TLS.
    if lower.starts_with(b"rediss://") || lower.starts_with(b"valkeys://") {
        let scheme = if lower.starts_with(b"rediss") { &b"rediss://"[..] } else { b"valkeys://" };
        return Some(fail(&[
            b"kevy-cli: ",
            scheme,
            b" is not supported: kevy-cli does not implement TLS",
        ]));
    }
    let rest = if lower.starts_with(b"redis://") {
        &uri[8..]
    } else if lower.starts_with(b"valkey://") {
        &uri[9..]
    } else {
        return Some(fail(&[b"Invalid URI scheme"]));
    };
    let rest = match userinfo(o, rest) {
        Ok(r) => r,
        Err(step) => return Some(step),
    };
    let path = rest.iter().position(|&b| b == b'/');
    if rest.first().is_some_and(|&b| b != b'/') {
        host_port(o, &rest[..path.unwrap_or(rest.len())]);
    }
    if let Some(p) = path
        && p + 1 < rest.len()
    {
        o.input_dbnum = atoi(&rest[p + 1..]);
    }
    if !(0..=65535).contains(&o.port) {
        return Some(fail(&[b"Invalid server port."]));
    }
    None
}

/// `[[user:]pass@]`: an empty user means legacy `AUTH pass`, an empty
/// password means no AUTH at all.
fn userinfo<'a>(o: &mut Opts, rest: &'a [u8]) -> Result<&'a [u8], Step> {
    let Some(at) = rest.iter().position(|&b| b == b'@') else {
        return Ok(rest);
    };
    let mut cur = rest;
    if let Some(colon) = rest.iter().position(|&b| b == b':')
        && colon < at
    {
        o.user = if colon > 0 { Some(percent_decode(&rest[..colon])?) } else { None };
        cur = &rest[colon + 1..];
    }
    let pass_len = at - (rest.len() - cur.len());
    o.auth = if pass_len > 0 { Some(percent_decode(&cur[..pass_len])?) } else { None };
    Ok(&rest[at + 1..])
}

/// `host[:port]` or `[ipv6][:port]`.
fn host_port(o: &mut Opts, authority: &[u8]) {
    if let Some(inner) = authority.strip_prefix(b"[") {
        if let Some(close) = inner.iter().position(|&b| b == b']') {
            if inner.get(close + 1) == Some(&b':') {
                o.port = atoi(&inner[close + 2..]);
            }
            o.host = inner[..close].to_vec();
            return;
        }
        o.host = inner.to_vec();
        return;
    }
    match authority.iter().position(|&b| b == b':') {
        Some(colon) => {
            o.port = atoi(&authority[colon + 1..]);
            o.host = authority[..colon].to_vec();
        }
        None => o.host = authority.to_vec(),
    }
}

fn percent_decode(s: &[u8]) -> Result<Vec<u8>, Step> {
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        if s[i] != b'%' {
            out.push(s[i]);
            i += 1;
            continue;
        }
        // redis-cli checks for two bytes after `%` counting from the `%`
        // itself, so `%2` at the end of a component reads the delimiter that
        // follows it (`:` or `@`, never a hex digit) and reports an illegal
        // character, not an incomplete encoding.
        if s.len() - i < 2 {
            return Err(fail(&[b"Incomplete URI encoding"]));
        }
        let hi = (s[i + 1] as char).to_ascii_lowercase().to_digit(16);
        let lo = s.get(i + 2).and_then(|&b| (b as char).to_ascii_lowercase().to_digit(16));
        match (hi, lo) {
            (Some(h), Some(l)) => out.push((h * 16 + l) as u8),
            _ => return Err(fail(&[b"Illegal character in URI encoding"])),
        }
        i += 3;
    }
    Ok(out)
}
