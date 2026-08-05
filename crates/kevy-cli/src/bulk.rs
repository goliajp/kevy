//! Prefix bulk ops + diagnostics:
//! `copy-prefix` / `delete-prefix` (token-bucket rate limit,
//! `--dry-run`), `digest`, `diff`, `inspect`.

use std::io::{self, Write};
use std::time::{Duration, Instant};

use kevy_resp::Reply;
use kevy_resp_client::RespClient;

/// Token bucket: `rate` ops/second, starting EMPTY (strict pacing —
/// a full-bucket start lets a small job burn its whole burst
/// unthrottled, defeating the point of `--rate` for short sweeps).
/// `rate == 0` = unlimited.
pub struct RateLimiter {
    rate: u64,
    tokens: f64,
    last: Instant,
}

impl RateLimiter {
    /// New limiter at `rate` ops/s (0 = off).
    pub fn new(rate: u64) -> Self {
        Self { rate, tokens: 0.0, last: Instant::now() }
    }

    /// Block until one op is admitted.
    pub fn take(&mut self) {
        if self.rate == 0 {
            return;
        }
        loop {
            let now = Instant::now();
            self.tokens = (self.tokens + now.duration_since(self.last).as_secs_f64() * self.rate as f64)
                .min(self.rate as f64);
            self.last = now;
            if self.tokens >= 1.0 {
                self.tokens -= 1.0;
                return;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}

fn scan_page(client: &mut RespClient, cursor: &[u8], pattern: &[u8]) -> io::Result<(Vec<u8>, Vec<Vec<u8>>)> {
    let reply = client.request_borrowed(&[b"SCAN", cursor, b"MATCH", pattern, b"COUNT", b"512"])?;
    let Reply::Array(items) = reply else {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "SCAN reply shape"));
    };
    let (Some(Reply::Bulk(next)), Some(Reply::Array(keys))) = (items.first(), items.get(1)) else {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "SCAN reply shape"));
    };
    let keys = keys
        .iter()
        .filter_map(|k| if let Reply::Bulk(b) = k { Some(b.clone()) } else { None })
        .collect();
    Ok((next.clone(), keys))
}

/// `delete-prefix`: SCAN + UNLINK, rate-limited. Returns deleted count.
pub fn run_delete_prefix(
    client: &mut RespClient,
    prefix: &[u8],
    rate: u64,
    dry_run: bool,
) -> io::Result<u64> {
    let mut pattern = prefix.to_vec();
    pattern.push(b'*');
    let mut cursor: Vec<u8> = b"0".to_vec();
    let mut limiter = RateLimiter::new(rate);
    let mut n = 0u64;
    loop {
        let (next, keys) = scan_page(client, &cursor, &pattern)?;
        for key in &keys {
            if dry_run {
                n += 1;
                continue;
            }
            limiter.take();
            if let Reply::Int(d) = client.request_borrowed(&[b"UNLINK", key])? {
                n += d as u64;
            }
        }
        cursor = next;
        if cursor == b"0" {
            return Ok(n);
        }
    }
}

/// `copy-prefix`: SCAN src prefix, re-key under dst prefix via
/// read+rebuild frames (the server has no COPY verb; TTL carried as
/// absolute PEXPIREAT). Rate-limited per source key.
///
/// Returns what it copied **and what it could not** — same contract as
/// `export`, for the same reason: the rebuild set does not cover every
/// type, and a copy that quietly drops one is worse than a refusal.
pub fn run_copy_prefix(
    client: &mut RespClient,
    src_prefix: &[u8],
    dst_prefix: &[u8],
    rate: u64,
) -> io::Result<crate::migrate::Export> {
    let mut pattern = src_prefix.to_vec();
    pattern.push(b'*');
    let mut cursor: Vec<u8> = b"0".to_vec();
    let mut limiter = RateLimiter::new(rate);
    let mut n = 0u64;
    // Same skip as `export`, and it must be as loud: a copy that leaves
    // a type behind and says "copied N keys" is the same silence.
    let mut skipped: std::collections::BTreeMap<Vec<u8>, u64> = Default::default();
    loop {
        let (next, keys) = scan_page(client, &cursor, &pattern)?;
        for key in &keys {
            limiter.take();
            let mut dst = dst_prefix.to_vec();
            dst.extend_from_slice(&key[src_prefix.len()..]);
            let frames = match crate::migrate::rebuild_frames(client, key, &dst)? {
                crate::migrate::Rebuilt::Frames(f) => f,
                crate::migrate::Rebuilt::Vanished => continue,
                crate::migrate::Rebuilt::UnsupportedType(ty) => {
                    *skipped.entry(ty).or_insert(0u64) += 1;
                    continue;
                }
            };
            let n_cmds = count_commands(&frames);
            for r in client.pipeline_raw(&frames, n_cmds)? {
                if let Reply::Error(e) = r {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        String::from_utf8_lossy(&e).into_owned(),
                    ));
                }
            }
            n += 1;
        }
        cursor = next;
        if cursor == b"0" {
            return Ok(crate::migrate::Export { keys: n, skipped });
        }
    }
}

/// Number of RESP commands in a frame buffer (top-level '*' headers).
fn count_commands(mut b: &[u8]) -> usize {
    let mut n = 0;
    while let Some(len) = crate::migrate::command_len_pub(b) {
        n += 1;
        b = &b[len..];
    }
    n
}

/// `digest <prefix>` → (count, hex).
pub fn run_digest(client: &mut RespClient, prefix: &[u8]) -> io::Result<(i64, String)> {
    let r = client.request_borrowed(&[b"PREFIX.DIGEST", prefix])?;
    let Reply::Array(items) = r else {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "PREFIX.DIGEST reply"));
    };
    let (Some(Reply::Int(n)), Some(Reply::Bulk(hex))) = (items.first(), items.get(1)) else {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "PREFIX.DIGEST reply"));
    };
    Ok((*n, String::from_utf8_lossy(hex).into_owned()))
}

/// `diff`: compare prefixes across two servers. Returns mismatching
/// prefixes.
pub fn run_diff(
    a: &mut RespClient,
    b: &mut RespClient,
    prefixes: &[Vec<u8>],
    out: &mut impl Write,
) -> io::Result<Vec<Vec<u8>>> {
    let mut bad = Vec::new();
    for p in prefixes {
        let (na, da) = run_digest(a, p)?;
        let (nb, db) = run_digest(b, p)?;
        let ok = na == nb && da == db;
        writeln!(
            out,
            "{}  A: {na} keys {da}  B: {nb} keys {db}  {}",
            String::from_utf8_lossy(p),
            if ok { "OK" } else { "MISMATCH" }
        )?;
        if !ok {
            bad.push(p.clone());
        }
    }
    Ok(bad)
}

/// `inspect <prefix>`: sample keys, type distribution, sizes.
pub fn run_inspect(client: &mut RespClient, prefix: &[u8], out: &mut impl Write) -> io::Result<()> {
    let mut pattern = prefix.to_vec();
    pattern.push(b'*');
    let mut cursor: Vec<u8> = b"0".to_vec();
    let mut total = 0u64;
    let mut by_type: Vec<(String, u64)> = Vec::new();
    let mut samples: Vec<String> = Vec::new();
    loop {
        let (next, keys) = scan_page(client, &cursor, &pattern)?;
        for key in &keys {
            total += 1;
            if samples.len() < 8 {
                samples.push(String::from_utf8_lossy(key).into_owned());
            }
            if let Reply::Simple(t) = client.request_borrowed(&[b"TYPE", key])? {
                let t = String::from_utf8_lossy(&t).into_owned();
                match by_type.iter_mut().find(|(n, _)| *n == t) {
                    Some((_, c)) => *c += 1,
                    None => by_type.push((t, 1)),
                }
            }
        }
        cursor = next;
        if cursor == b"0" {
            break;
        }
    }
    writeln!(out, "prefix {}: {total} keys", String::from_utf8_lossy(prefix))?;
    by_type.sort_by_key(|(_, c)| std::cmp::Reverse(*c));
    for (t, c) in &by_type {
        writeln!(out, "  {t}: {c}")?;
    }
    if !samples.is_empty() {
        writeln!(out, "  samples: {}", samples.join(", "))?;
    }
    Ok(())
}
