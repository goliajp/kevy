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
}

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
        })
    }

    /// Number of replica connections.
    pub fn replica_count(&self) -> usize {
        self.replicas.len()
    }

    /// Route a write command (or any command that must hit the
    /// primary) to the primary connection.
    pub fn request_write(&mut self, args: &[Vec<u8>]) -> io::Result<Reply> {
        self.primary.request(args)
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
}
