//! kevy-cluster-rw — read/write-split cluster client for kevy.
//!
//! Splits each client command between a **primary** connection (for
//! writes — keyspace-mutating verbs) and a fleet of **replica**
//! connections (for reads — round-robin across them, fallback to the
//! primary when no replica is configured). A per-command
//! `consistent: bool` knob (`READCONSISTENT` semantics) forces a read
//! to the primary for callers that need fresh data.
//!
//! v1.18 model: the operator supplies the primary address + a list of
//! replica addresses to [`ReadWriteClient::connect`]; the client
//! holds one `RespClient` per node. Server-side classification is
//! intentionally not consulted (no implicit `CLUSTER SLOTS` walk);
//! this keeps the client correct under both `[cluster] enabled` and
//! standalone deployments.
//!
//! Per-command read/write classification lives in [`is_write_verb`]
//! — a small static table mirroring `kevy::cmd`'s server-side rule
//! (duplicated here on purpose: this crate is downstream of
//! `kevy-resp-client` only, so it never depends on the server crate).
#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::collections::HashMap;
use std::io;

use kevy_resp::Reply;
use kevy_resp_client::RespClient;

/// Read/write-split cluster client. Owns one `RespClient` to the
/// primary node + one per replica node. Round-robins reads across
/// the replica fleet (fallback to primary on empty fleet or
/// `consistent = true`).
pub struct ReadWriteClient {
    primary: RespClient,
    replicas: Vec<RespClient>,
    /// Wrap-around counter for read load-balancing across replicas.
    /// Even distribution under steady-state; the rare write-immediately-
    /// after-write pattern that pins to a single replica is acceptable
    /// for v1.18 (no fairness guarantee).
    rr_counter: usize,
    /// v3-cluster Phase 3 scope cache: `host:port` → live
    /// `RespClient`. Populated on demand when a write returns
    /// `-MISDIRECTED writer is <host:port>` — the client opens a
    /// new connection, caches it, and retries against the named
    /// writer.
    scope_writers: HashMap<String, RespClient>,
    /// Per-key target cache: key bytes → `host:port` of the writer
    /// most recently confirmed by the server. Subsequent writes for
    /// the same key skip the primary round-trip + go straight to
    /// the cached writer. Bounded at 4096 entries; the oldest get
    /// dropped wholesale when exceeded (the cache is rebuilt by
    /// the next `-MISDIRECTED` reply, so we don't need an LRU).
    /// Prefix-shape caching is a follow-up — it requires the
    /// server to include the prefix in the MISDIRECTED reply.
    scope_key_targets: HashMap<Vec<u8>, String>,
}

/// Cap for [`ReadWriteClient::scope_key_targets`] before bulk-evict.
const SCOPE_KEY_CACHE_CAP: usize = 4096;

/// Max retries the client attempts after a `-QUIESCED` reply
/// (T3.15). Total worst-case wait ≈
/// `QUIESCE_RETRY_MIN_MS * (2^N - 1)` with N = budget; with
/// 5 ms / 80 ms / budget = 7 that's ~635 ms ceiling — enough
/// to ride out a typical KB-sized scope's quiesce window.
const QUIESCE_RETRY_BUDGET: usize = 7;
const QUIESCE_RETRY_MIN_MS: u64 = 5;
const QUIESCE_RETRY_MAX_MS: u64 = 80;

impl ReadWriteClient {
    /// Open one connection to the primary and one per replica.
    ///
    /// `primary` is the address of the kevy node running with
    /// `[replication] role = "primary"` (or `standalone` — both
    /// behave the same from the client's perspective). `replicas`
    /// lists the addresses of kevy nodes running with
    /// `[replication] role = "replica"`; v1.18 assumes they all
    /// share the keyspace of the primary (operator-enforced — kevy
    /// has no automatic discovery in v1.18).
    pub fn connect(primary: (&str, u16), replicas: &[(&str, u16)]) -> io::Result<Self> {
        let primary_conn = RespClient::connect(primary.0, primary.1)?;
        let mut replica_conns = Vec::with_capacity(replicas.len());
        for (host, port) in replicas {
            replica_conns.push(RespClient::connect(host, *port)?);
        }
        Ok(Self {
            primary: primary_conn,
            replicas: replica_conns,
            rr_counter: 0,
            scope_writers: HashMap::new(),
            scope_key_targets: HashMap::new(),
        })
    }

    /// Number of replica connections.
    pub fn replica_count(&self) -> usize {
        self.replicas.len()
    }

    /// Route a write command (or any command that must hit the
    /// primary) to the primary connection. If the server replies
    /// `-MISDIRECTED writer is <host:port>` (Phase 3 / v1.21 scoped
    /// multi-writer), the client transparently opens a connection
    /// to the named writer (caching it), caches the key→writer
    /// mapping for follow-up writes on the same key, and retries
    /// **once**. A second `-MISDIRECTED` from the retry surfaces as
    /// an error.
    pub fn request_write(&mut self, args: &[Vec<u8>]) -> io::Result<Reply> {
        // Fast path: a prior MISDIRECTED for this key cached its
        // writer's address — skip the primary round-trip.
        if let Some(key) = args.get(1)
            && let Some(addr) = self.scope_key_targets.get(key.as_slice()).cloned()
        {
            return self.request_via_writer(&addr, args);
        }
        self.request_write_with_quiesce_retry(args)
    }

    /// Send `args` to the primary; on `-QUIESCED` (T3.15), back off
    /// and retry up to `QUIESCE_RETRY_BUDGET` times against the same
    /// primary. The migration is in-flight; the cluster member that
    /// answered will start returning `-MISDIRECTED` once the
    /// migration commits, and the client follows via the existing
    /// MISDIRECTED branch. Surfaces the final `-QUIESCED` reply as
    /// a regular `Reply::Error` after exhausting retries — the
    /// caller decides whether to back off further or fail.
    fn request_write_with_quiesce_retry(&mut self, args: &[Vec<u8>]) -> io::Result<Reply> {
        let mut backoff = std::time::Duration::from_millis(QUIESCE_RETRY_MIN_MS);
        for _ in 0..QUIESCE_RETRY_BUDGET {
            let reply = self.primary.request(args)?;
            if let Some(target_addr) = parse_misdirected(&reply) {
                if let Some(key) = args.get(1) {
                    self.remember_key_target(key, &target_addr);
                }
                return self.request_via_writer(&target_addr, args);
            }
            if parse_quiesced(&reply).is_some() {
                // Migration in flight — back off and retry. Do NOT
                // cache the QUIESCED `<to-addr>`: the migration may
                // abort and the original writer would resume.
                std::thread::sleep(backoff);
                backoff = (backoff * 2).min(std::time::Duration::from_millis(QUIESCE_RETRY_MAX_MS));
                continue;
            }
            return Ok(reply);
        }
        // Exhausted the retry budget. Final attempt; whatever it
        // returns (likely another -QUIESCED) bubbles to the caller.
        self.primary.request(args)
    }

    /// Open + cache a connection to `addr` (`"host:port"`) and send
    /// `args`. A second `-MISDIRECTED` from this hop is **not**
    /// followed — the client would be in a redirect loop and the
    /// caller deserves to see the error rather than burn round-trips.
    fn request_via_writer(&mut self, addr: &str, args: &[Vec<u8>]) -> io::Result<Reply> {
        if !self.scope_writers.contains_key(addr) {
            let (host, port) = split_host_port(addr).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("server returned MISDIRECTED with malformed target {addr:?}"),
                )
            })?;
            let conn = RespClient::connect(host, port)?;
            self.scope_writers.insert(addr.to_string(), conn);
        }
        let conn = self
            .scope_writers
            .get_mut(addr)
            .expect("just inserted above");
        conn.request(args)
    }

    fn remember_key_target(&mut self, key: &[u8], addr: &str) {
        if self.scope_key_targets.len() >= SCOPE_KEY_CACHE_CAP {
            // Bulk-evict — next MISDIRECTED will reseed. The
            // cache is an optimisation; correctness still holds.
            self.scope_key_targets.clear();
        }
        self.scope_key_targets.insert(key.to_vec(), addr.to_string());
    }

    /// Route a read command to a replica (round-robin); fallback to
    /// the primary when no replica is configured or when
    /// `consistent` is `true`.
    pub fn request_read(&mut self, args: &[Vec<u8>], consistent: bool) -> io::Result<Reply> {
        if consistent || self.replicas.is_empty() {
            return self.primary.request(args);
        }
        let idx = self.rr_counter % self.replicas.len();
        self.rr_counter = self.rr_counter.wrapping_add(1);
        self.replicas[idx].request(args)
    }

    /// Auto-routed command. Classifies `args[0]` via [`is_write_verb`]
    /// and dispatches to either [`Self::request_write`] or
    /// [`Self::request_read`] (`consistent = false`). Convenience for
    /// callers that don't want to make the read/write decision
    /// explicit.
    pub fn request(&mut self, args: &[Vec<u8>]) -> io::Result<Reply> {
        let Some(verb) = args.first() else {
            return self.primary.request(args);
        };
        if is_write_verb(verb) {
            self.request_write(args)
        } else {
            self.request_read(args, false)
        }
    }
}

/// `true` when `verb` is a keyspace-mutating command and so must run
/// against the primary. Otherwise the command is read-side and a
/// replica can serve it.
///
/// The table mirrors `kevy::cmd::is_write_verb` (server-side) — kept
/// in sync by review. Verbs not listed here (including PING / ECHO /
/// CLUSTER / CLIENT / HELLO) are read-side or keyspace-neutral.
pub fn is_write_verb(verb: &[u8]) -> bool {
    let mut buf = [0u8; 32];
    let upper = ascii_upper(verb, &mut buf);
    matches!(
        upper,
        // strings + counters
        b"SET" | b"SETNX" | b"SETEX" | b"PSETEX" | b"MSET" | b"MSETNX"
        | b"APPEND" | b"INCR" | b"INCRBY" | b"INCRBYFLOAT"
        | b"DECR" | b"DECRBY" | b"GETSET" | b"GETDEL"
        | b"SETRANGE"
        // generic keyspace
        | b"DEL" | b"UNLINK" | b"EXPIRE" | b"EXPIREAT" | b"PEXPIRE" | b"PEXPIREAT"
        | b"PERSIST" | b"RENAME" | b"RENAMENX" | b"TYPE" // TYPE is read but cheap to misclassify
        | b"COPY" | b"OBJECT"
        // hash
        | b"HSET" | b"HSETNX" | b"HMSET" | b"HDEL" | b"HINCRBY" | b"HINCRBYFLOAT"
        // list
        | b"LPUSH" | b"RPUSH" | b"LPUSHX" | b"RPUSHX" | b"LPOP" | b"RPOP"
        | b"LREM" | b"LTRIM" | b"LSET" | b"LINSERT" | b"RPOPLPUSH" | b"LMOVE"
        | b"BLPOP" | b"BRPOP" | b"BLMOVE"
        // set
        | b"SADD" | b"SREM" | b"SPOP" | b"SMOVE" | b"SINTERSTORE" | b"SUNIONSTORE" | b"SDIFFSTORE"
        // zset
        | b"ZADD" | b"ZREM" | b"ZINCRBY" | b"ZPOPMIN" | b"ZPOPMAX"
        | b"ZREMRANGEBYRANK" | b"ZREMRANGEBYSCORE" | b"ZREMRANGEBYLEX"
        // stream
        | b"XADD" | b"XDEL" | b"XTRIM" | b"XGROUP" | b"XACK" | b"XCLAIM" | b"XAUTOCLAIM"
        // server admin (mutates state)
        | b"FLUSHDB" | b"FLUSHALL" | b"CONFIG" | b"SAVE" | b"BGSAVE" | b"BGREWRITEAOF"
        | b"REPLICAOF" | b"SLAVEOF"
        // pub/sub PUBLISH technically mutates subscriber state — route to primary
        // so the publisher's "delivered count" reflects the primary's registry
        | b"PUBLISH" | b"SPUBLISH"
        // txn
        | b"MULTI" | b"EXEC" | b"DISCARD" | b"WATCH" | b"UNWATCH"
    )
}

fn ascii_upper<'a>(s: &[u8], buf: &'a mut [u8; 32]) -> &'a [u8] {
    let n = s.len().min(32);
    for i in 0..n {
        buf[i] = s[i].to_ascii_uppercase();
    }
    &buf[..n]
}

/// Detect a Phase 3 `-MISDIRECTED writer is <host:port>` reply and
/// extract the host-port target. `None` for any other Reply (incl.
/// non-MISDIRECTED `-ERR ...` strings) — caller propagates those
/// unchanged.
fn parse_misdirected(reply: &Reply) -> Option<String> {
    let Reply::Error(bytes) = reply else { return None };
    // Expect `MISDIRECTED writer is <addr>` — kevy server's
    // `scope_integration::encode_misdirected` shape.
    const PREFIX: &[u8] = b"MISDIRECTED writer is ";
    if !bytes.starts_with(PREFIX) {
        return None;
    }
    let addr = std::str::from_utf8(&bytes[PREFIX.len()..]).ok()?;
    let addr = addr.trim_end_matches(['\r', '\n']);
    if addr.is_empty() {
        return None;
    }
    Some(addr.to_string())
}

/// T3.15: detect a `-QUIESCED migrating to <host:port>` reply and
/// extract the target. Same shape rationale as
/// [`parse_misdirected`]; clients use this to know "back off + retry
/// against the original writer until the migration commits".
fn parse_quiesced(reply: &Reply) -> Option<String> {
    let Reply::Error(bytes) = reply else { return None };
    const PREFIX: &[u8] = b"QUIESCED migrating to ";
    if !bytes.starts_with(PREFIX) {
        return None;
    }
    let addr = std::str::from_utf8(&bytes[PREFIX.len()..]).ok()?;
    let addr = addr.trim_end_matches(['\r', '\n']);
    if addr.is_empty() {
        return None;
    }
    Some(addr.to_string())
}

/// Parse `"host:port"`. Rejects empty host or non-u16 port.
fn split_host_port(addr: &str) -> Option<(&str, u16)> {
    let colon = addr.rfind(':')?;
    let host = &addr[..colon];
    if host.is_empty() {
        return None;
    }
    let port: u16 = addr[colon + 1..].parse().ok()?;
    Some((host, port))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_classified_correctly() {
        for verb in [&b"SET"[..], b"DEL", b"LPUSH", b"HSET", b"ZADD", b"XADD", b"FLUSHDB", b"REPLICAOF"] {
            assert!(is_write_verb(verb), "{:?} should be write", std::str::from_utf8(verb));
        }
    }

    #[test]
    fn reads_classified_correctly() {
        for verb in [&b"GET"[..], b"HGET", b"LRANGE", b"SMEMBERS", b"ZSCORE", b"XRANGE", b"PING", b"INFO"] {
            assert!(!is_write_verb(verb), "{:?} should be read", std::str::from_utf8(verb));
        }
    }

    #[test]
    fn classification_is_case_insensitive() {
        assert!(is_write_verb(b"set"));
        assert!(is_write_verb(b"Set"));
        assert!(is_write_verb(b"SET"));
        assert!(!is_write_verb(b"get"));
        assert!(!is_write_verb(b"Get"));
    }

    #[test]
    fn long_verb_doesnt_panic_on_classification() {
        // Verbs longer than 32 bytes (silly but legal RESP) are
        // truncated by the upper-buf — they fall through to the
        // catch-all read classification.
        assert!(!is_write_verb(&[b'X'; 64]));
    }

    // ---- scope MISDIRECTED parser (T3.9) ----

    #[test]
    fn parse_misdirected_basic() {
        let r = Reply::Error(b"MISDIRECTED writer is 10.0.0.1:6004".to_vec());
        assert_eq!(parse_misdirected(&r).as_deref(), Some("10.0.0.1:6004"));
    }

    #[test]
    fn parse_misdirected_strips_trailing_crlf() {
        // Some encoders leave `\r\n` in the Error payload; parser
        // tolerates both shapes.
        let r = Reply::Error(b"MISDIRECTED writer is 10.0.0.1:6004\r\n".to_vec());
        assert_eq!(parse_misdirected(&r).as_deref(), Some("10.0.0.1:6004"));
    }

    #[test]
    fn parse_misdirected_rejects_unrelated_error() {
        let r = Reply::Error(b"ERR something else".to_vec());
        assert!(parse_misdirected(&r).is_none());
        // Non-Error replies are also rejected.
        let r = Reply::Simple(b"OK".to_vec());
        assert!(parse_misdirected(&r).is_none());
    }

    #[test]
    fn split_host_port_dotted_v4_and_dns() {
        assert_eq!(split_host_port("10.0.0.1:6004"), Some(("10.0.0.1", 6004)));
        assert_eq!(split_host_port("db.local:6105"), Some(("db.local", 6105)));
    }

    #[test]
    fn parse_quiesced_basic() {
        let r = Reply::Error(b"QUIESCED migrating to 10.0.0.1:6004".to_vec());
        assert_eq!(parse_quiesced(&r).as_deref(), Some("10.0.0.1:6004"));
    }

    #[test]
    fn parse_quiesced_strips_trailing_crlf() {
        let r = Reply::Error(b"QUIESCED migrating to 10.0.0.1:6004\r\n".to_vec());
        assert_eq!(parse_quiesced(&r).as_deref(), Some("10.0.0.1:6004"));
    }

    #[test]
    fn parse_quiesced_rejects_unrelated_error() {
        let r = Reply::Error(b"MISDIRECTED writer is 10.0.0.1:6004".to_vec());
        assert!(parse_quiesced(&r).is_none());
        let r = Reply::Simple(b"OK".to_vec());
        assert!(parse_quiesced(&r).is_none());
    }

    #[test]
    fn split_host_port_rejects_bad_inputs() {
        assert!(split_host_port("nohost:").is_none());
        assert!(split_host_port(":6004").is_none());
        assert!(split_host_port("no-colon").is_none());
        assert!(split_host_port("host:99999").is_none()); // u16 overflow
    }
}
