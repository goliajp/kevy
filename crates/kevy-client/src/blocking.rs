//! Blocking pops: `BLPOP` / `BRPOP` / `BZPOPMIN` (v1.14.0).
//!
//! Both backends block for real. The embedded store parks the calling
//! thread on the store's process-wide condvar (kevy-embedded v2.4
//! blocking ops); the remote backend sends the verb and the *server*
//! parks the connection until data or timeout (kevy v2.4 BLOCK
//! reactor).
//!
//! Read-timeout note (remote): [`kevy_resp_client::RespClient`] sets
//! **no socket read timeout** — the reply read blocks as long as the
//! server holds the connection, so a blocking pop never races a
//! client-side read deadline. The flip side: `timeout = None` (wire
//! `0`) occupies the connection indefinitely; don't share that
//! connection with latency-sensitive traffic.

use std::io;
use std::time::Duration;

use kevy_resp::Reply;
use kevy_resp_client::RespClient;

use crate::{Connection, num_f64, string, unexpected};

/// `(key, member, score)` from [`Connection::bzpopmin`].
pub type ZPopHit = (Vec<u8>, Vec<u8>, f64);

impl Connection {
    /// `BLPOP key [key ...] timeout` — block until one of `keys` has a
    /// head element (checked in argument order), pop it, and return
    /// `(key, value)`. `None` = timed out with every list still empty.
    ///
    /// `timeout = None` waits forever. `Some(Duration::ZERO)` is
    /// rejected with `InvalidInput`: the wire encodes `0` as "wait
    /// forever", so a zero duration cannot mean "poll once".
    pub fn blpop(
        &mut self,
        keys: &[&[u8]],
        timeout: Option<Duration>,
    ) -> io::Result<Option<(Vec<u8>, Vec<u8>)>> {
        check_blocking_args(keys, timeout)?;
        match self {
            Self::Embedded(s) => s.blpop(keys, timeout),
            Self::Remote(c) => pop_kv(blocking_request(c, b"BLPOP", keys, timeout)?),
        }
    }

    /// `BRPOP key [key ...] timeout` — symmetric to [`Self::blpop`]
    /// from the tail.
    pub fn brpop(
        &mut self,
        keys: &[&[u8]],
        timeout: Option<Duration>,
    ) -> io::Result<Option<(Vec<u8>, Vec<u8>)>> {
        check_blocking_args(keys, timeout)?;
        match self {
            Self::Embedded(s) => s.brpop(keys, timeout),
            Self::Remote(c) => pop_kv(blocking_request(c, b"BRPOP", keys, timeout)?),
        }
    }

    /// `BZPOPMIN key [key ...] timeout` — block until one of the
    /// sorted sets has a member, then pop the lowest-scored one.
    /// Returns `(key, member, score)`; `None` on timeout. Timeout
    /// semantics as in [`Self::blpop`]. (The server has no `BZPOPMAX`;
    /// see docs/verb-reference.md.)
    pub fn bzpopmin(
        &mut self,
        keys: &[&[u8]],
        timeout: Option<Duration>,
    ) -> io::Result<Option<ZPopHit>> {
        check_blocking_args(keys, timeout)?;
        match self {
            Self::Embedded(s) => s.bzpopmin(keys, timeout),
            Self::Remote(c) => pop_kv_score(blocking_request(c, b"BZPOPMIN", keys, timeout)?),
        }
    }
}

/// Shared contract gate: at least one key, and no zero `Some` timeout
/// (wire `0` means forever — a zero duration would silently invert
/// into an infinite wait on the remote backend).
fn check_blocking_args(keys: &[&[u8]], timeout: Option<Duration>) -> io::Result<()> {
    if keys.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "blocking pop needs at least one key",
        ));
    }
    if timeout == Some(Duration::ZERO) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "timeout Some(0) is ambiguous (wire 0 = wait forever); use None to wait forever",
        ));
    }
    Ok(())
}

/// Send `verb key… timeout` and return the raw reply. `None` → wire
/// `0` (forever); `Some(d)` → fractional seconds (the server parses
/// f64, ms precision).
fn blocking_request(
    c: &mut RespClient,
    verb: &[u8],
    keys: &[&[u8]],
    timeout: Option<Duration>,
) -> io::Result<Reply> {
    let mut args = Vec::with_capacity(keys.len() + 2);
    args.push(verb.to_vec());
    args.extend(keys.iter().map(|k| k.to_vec()));
    args.push(match timeout {
        None => b"0".to_vec(),
        Some(d) => format!("{}", d.as_secs_f64()).into_bytes(),
    });
    c.request(&args)
}

/// `*2 [key, value]` → `Some`; nil array (RESP2 timeout shape) → `None`.
fn pop_kv(reply: Reply) -> io::Result<Option<(Vec<u8>, Vec<u8>)>> {
    match reply {
        Reply::Array(items) if items.len() == 2 => {
            let mut it = items.into_iter();
            match (it.next().unwrap(), it.next().unwrap()) {
                (Reply::Bulk(k), Reply::Bulk(v)) => Ok(Some((k, v))),
                (a, _) => Err(unexpected(a)),
            }
        }
        Reply::Nil | Reply::Null => Ok(None),
        Reply::Error(e) => Err(io::Error::other(string(e))),
        other => Err(unexpected(other)),
    }
}

/// `*3 [key, member, score]` → `Some`; nil array → `None`.
fn pop_kv_score(reply: Reply) -> io::Result<Option<ZPopHit>> {
    match reply {
        Reply::Array(items) if items.len() == 3 => {
            let mut it = items.into_iter();
            match (it.next().unwrap(), it.next().unwrap(), it.next().unwrap()) {
                (Reply::Bulk(k), Reply::Bulk(m), Reply::Bulk(s)) => {
                    Ok(Some((k, m, num_f64(&s)?)))
                }
                (a, _, _) => Err(unexpected(a)),
            }
        }
        Reply::Nil | Reply::Null => Ok(None),
        Reply::Error(e) => Err(io::Error::other(string(e))),
        other => Err(unexpected(other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_blpop_immediate_hit() {
        let mut c = Connection::open("mem://").unwrap();
        c.rpush(b"q", &[&b"a"[..], &b"b"[..]]).unwrap();
        let hit = c.blpop(&[&b"q"[..]], Some(Duration::from_secs(1))).unwrap();
        assert_eq!(hit, Some((b"q".to_vec(), b"a".to_vec())));
        let hit = c.brpop(&[&b"q"[..]], Some(Duration::from_secs(1))).unwrap();
        assert_eq!(hit, Some((b"q".to_vec(), b"b".to_vec())));
    }

    #[test]
    fn embedded_blpop_timeout_returns_none() {
        let mut c = Connection::open("mem://").unwrap();
        let hit = c
            .blpop(&[&b"empty"[..]], Some(Duration::from_millis(30)))
            .unwrap();
        assert_eq!(hit, None);
    }

    #[test]
    fn embedded_bzpopmin_pops_lowest_score() {
        let mut c = Connection::open("mem://").unwrap();
        c.zadd(b"z", &[(2.0, b"hi".as_ref()), (1.0, b"lo".as_ref())])
            .unwrap();
        let hit = c
            .bzpopmin(&[&b"z"[..]], Some(Duration::from_secs(1)))
            .unwrap();
        assert_eq!(hit, Some((b"z".to_vec(), b"lo".to_vec(), 1.0)));
        let miss = c
            .bzpopmin(&[&b"gone"[..]], Some(Duration::from_millis(30)))
            .unwrap();
        assert_eq!(miss, None);
    }

    #[test]
    fn zero_some_timeout_and_empty_keys_rejected() {
        let mut c = Connection::open("mem://").unwrap();
        let err = c.blpop(&[&b"q"[..]], Some(Duration::ZERO)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        let err = c.blpop(&[], Some(Duration::from_secs(1))).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }
}
