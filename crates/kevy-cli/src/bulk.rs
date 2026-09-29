//! Prefix bulk ops + diagnostics:
//! `copy-prefix` / `delete-prefix` (token-bucket rate limit,
//! `--dry-run`), `digest`, `diff`, `inspect`.
//!
//! ```
//! use kevy_cli::bulk::{DeleteMode, run_copy_prefix, run_delete_prefix, run_digest};
//! # let port = include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/doc_server/serve.rs"));
//! let mut client = kevy_resp_client::RespClient::connect("127.0.0.1", port)?;
//! client.request_borrowed(&[b"SET", b"old:1", b"a"])?;
//! client.request_borrowed(&[b"SADD", b"old:2", b"x", b"y"])?;
//! // move a prefix: copy, prove it arrived, then drop the source
//! run_copy_prefix(&mut client, b"old:", b"new:", 0)?;
//! assert_eq!(run_digest(&mut client, b"new:")?.0, 2);
//! assert_eq!(run_delete_prefix(&mut client, b"old:", 0, DeleteMode::Unlink)?, 2);
//! assert_eq!(run_digest(&mut client, b"old:")?.0, 0);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

use std::io::{self, Write};
use std::time::{Duration, Instant};

use crate::link::Link;
use kevy_resp::Reply;

/// Token bucket: `rate` ops/second, starting EMPTY (strict pacing —
/// a full-bucket start lets a small job burn its whole burst
/// unthrottled, defeating the point of `--rate` for short sweeps).
/// `rate == 0` = unlimited.
///
/// ```
/// use kevy_cli::bulk::RateLimiter;
/// use std::time::{Duration, Instant};
///
/// let mut limiter = RateLimiter::new(100); // 100 ops/s
/// let start = Instant::now();
/// for _ in 0..5 {
///     limiter.take();
/// }
/// // the bucket starts empty, so even a short burst is paced
/// assert!(start.elapsed() >= Duration::from_millis(40));
/// ```
#[derive(Debug)]
pub struct RateLimiter {
    rate: u64,
    tokens: f64,
    last: Instant,
}

impl RateLimiter {
    /// New limiter at `rate` ops/s (0 = off).
    ///
    /// ```
    /// use kevy_cli::bulk::RateLimiter;
    /// let mut unlimited = RateLimiter::new(0);
    /// let start = std::time::Instant::now();
    /// for _ in 0..10_000 {
    ///     unlimited.take(); // never blocks
    /// }
    /// assert!(start.elapsed() < std::time::Duration::from_secs(1));
    /// ```
    pub fn new(rate: u64) -> Self {
        Self { rate, tokens: 0.0, last: Instant::now() }
    }

    /// Block until one op is admitted.
    ///
    /// ```
    /// use kevy_cli::bulk::RateLimiter;
    /// use std::time::{Duration, Instant};
    ///
    /// let mut limiter = RateLimiter::new(50);
    /// let start = Instant::now();
    /// limiter.take(); // the first op already waits for its token (~20 ms)
    /// assert!(start.elapsed() >= Duration::from_millis(15));
    /// ```
    pub fn take(&mut self) {
        if self.rate == 0 {
            return;
        }
        loop {
            let now = Instant::now();
            self.tokens = (self.tokens
                + now.duration_since(self.last).as_secs_f64() * self.rate as f64)
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

fn scan_page(
    client: &mut dyn Link,
    cursor: &[u8],
    pattern: &[u8],
) -> io::Result<(Vec<u8>, Vec<Vec<u8>>)> {
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

/// Whether `delete-prefix` deletes what it finds or only counts it.
///
/// ```
/// use kevy_cli::bulk::DeleteMode;
/// // `delete-prefix --dry-run`
/// assert_ne!(DeleteMode::DryRun, DeleteMode::default());
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum DeleteMode {
    /// UNLINK every key found.
    ///
    /// ```
    /// use kevy_cli::bulk::{DeleteMode, run_delete_prefix};
    /// # let port = include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/doc_server/serve.rs"));
    /// let mut client = kevy_resp_client::RespClient::connect("127.0.0.1", port)?;
    /// client.request_borrowed(&[b"SET", b"tmp:a", b"1"])?;
    /// assert_eq!(run_delete_prefix(&mut client, b"tmp:", 0, DeleteMode::Unlink)?, 1);
    /// let left = client.request_borrowed(&[b"EXISTS", b"tmp:a"])?;
    /// assert_eq!(left, kevy_resp_client::Reply::Int(0));
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    #[default]
    Unlink,
    /// Count the keys that would be deleted and touch nothing
    /// (`--dry-run`).
    ///
    /// ```
    /// use kevy_cli::bulk::{DeleteMode, run_delete_prefix};
    /// # let port = include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/doc_server/serve.rs"));
    /// let mut client = kevy_resp_client::RespClient::connect("127.0.0.1", port)?;
    /// client.request_borrowed(&[b"SET", b"tmp:a", b"1"])?;
    /// assert_eq!(run_delete_prefix(&mut client, b"tmp:", 0, DeleteMode::DryRun)?, 1);
    /// let left = client.request_borrowed(&[b"EXISTS", b"tmp:a"])?;
    /// assert_eq!(left, kevy_resp_client::Reply::Int(1), "a dry run touches nothing");
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    DryRun,
}

/// `delete-prefix`: SCAN + UNLINK, rate-limited. Returns the deleted
/// count, or under [`DeleteMode::DryRun`] the count that would be.
///
/// ```
/// use kevy_cli::bulk::{DeleteMode, run_delete_prefix};
/// # let port = include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/doc_server/serve.rs"));
/// let mut client = kevy_resp_client::RespClient::connect("127.0.0.1", port)?;
/// for k in ["tmp:1", "tmp:2", "keep:1"] {
///     client.request_borrowed(&[b"SET", k.as_bytes(), b"v"])?;
/// }
/// let would = run_delete_prefix(&mut client, b"tmp:", 0, DeleteMode::DryRun)?;
/// assert_eq!(would, 2);
/// assert_eq!(run_delete_prefix(&mut client, b"tmp:", 1000, DeleteMode::Unlink)?, would);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub fn run_delete_prefix(
    client: &mut dyn Link,
    prefix: &[u8],
    rate: u64,
    mode: DeleteMode,
) -> io::Result<u64> {
    let mut pattern = prefix.to_vec();
    pattern.push(b'*');
    let mut cursor: Vec<u8> = b"0".to_vec();
    let mut limiter = RateLimiter::new(rate);
    let mut n = 0u64;
    loop {
        let (next, keys) = scan_page(client, &cursor, &pattern)?;
        for key in &keys {
            if mode == DeleteMode::DryRun {
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
///
/// ```
/// use kevy_cli::bulk::run_copy_prefix;
/// use kevy_resp_client::Reply;
/// # let port = include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/doc_server/serve.rs"));
/// let mut client = kevy_resp_client::RespClient::connect("127.0.0.1", port)?;
///
/// client.request_borrowed(&[b"HSET", b"src:1", b"name", b"ada"])?;
/// client.request_borrowed(&[b"SET", b"src:2", b"x"])?;
/// client.request_borrowed(&[b"PEXPIRE", b"src:2", b"60000"])?;
/// let copied = run_copy_prefix(&mut client, b"src:", b"dst:", 0)?;
/// assert_eq!(copied.keys, 2);
/// assert!(copied.skipped.is_empty(), "every type had a rebuild verb");
/// let name = client.request_borrowed(&[b"HGET", b"dst:1", b"name"])?;
/// assert_eq!(name, Reply::Bulk(b"ada".to_vec()));
/// // the TTL travels with the key
/// assert!(matches!(client.request_borrowed(&[b"PTTL", b"dst:2"])?, Reply::Int(ms) if ms > 0));
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub fn run_copy_prefix(
    client: &mut dyn Link,
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
///
/// ```
/// use kevy_cli::bulk::run_digest;
/// # let port = include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/doc_server/serve.rs"));
/// let mut client = kevy_resp_client::RespClient::connect("127.0.0.1", port)?;
/// client.request_borrowed(&[b"SET", b"cfg:a", b"1"])?;
/// let (count, before) = run_digest(&mut client, b"cfg:")?;
/// assert_eq!(count, 1);
/// client.request_borrowed(&[b"SET", b"cfg:a", b"2"])?;
/// let (_, after) = run_digest(&mut client, b"cfg:")?;
/// assert_ne!(before, after, "any change to a value changes the digest");
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub fn run_digest(client: &mut dyn Link, prefix: &[u8]) -> io::Result<(i64, String)> {
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
///
/// ```
/// use kevy_cli::bulk::run_diff;
/// # let port = include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/doc_server/serve.rs"));
/// let mut client = kevy_resp_client::RespClient::connect("127.0.0.1", port)?;
/// # let port_b = include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/doc_server/serve.rs"));
/// let mut replica = kevy_resp_client::RespClient::connect("127.0.0.1", port_b)?;
/// for c in [&mut client, &mut replica] {
///     c.request_borrowed(&[b"SET", b"user:1", b"ada"])?;
///     c.request_borrowed(&[b"SET", b"order:1", b"open"])?;
/// }
/// replica.request_borrowed(&[b"SET", b"order:1", b"shipped"])?;
///
/// let mut out = Vec::new();
/// let prefixes = [b"user:".to_vec(), b"order:".to_vec()];
/// let bad = run_diff(&mut client, &mut replica, &prefixes, &mut out)?;
/// assert_eq!(bad, vec![b"order:".to_vec()]);
/// assert!(String::from_utf8(out)?.lines().next().is_some_and(|l| l.ends_with("OK")));
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub fn run_diff(
    a: &mut dyn Link,
    b: &mut dyn Link,
    prefixes: &[Vec<u8>],
    mut out: impl Write,
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
///
/// ```
/// use kevy_cli::bulk::run_inspect;
/// # let port = include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/doc_server/serve.rs"));
/// let mut client = kevy_resp_client::RespClient::connect("127.0.0.1", port)?;
/// client.request_borrowed(&[b"SET", b"s:1", b"a"])?;
/// client.request_borrowed(&[b"SET", b"s:2", b"b"])?;
/// client.request_borrowed(&[b"SADD", b"s:3", b"m"])?;
/// let mut out = Vec::new();
/// run_inspect(&mut client, b"s:", &mut out)?;
/// let text = String::from_utf8(out)?;
/// assert!(text.starts_with("prefix s:: 3 keys\n  string: 2\n  set: 1\n"), "{text}");
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub fn run_inspect(client: &mut dyn Link, prefix: &[u8], mut out: impl Write) -> io::Result<()> {
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
