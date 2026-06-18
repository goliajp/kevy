//! `MULTI` / `EXEC` / `DISCARD` — Redis transactions, with optional
//! `WATCH`-driven optimistic concurrency (v1.5.0).
//!
//! Wire flow (Remote): client sends `MULTI` → server `+OK`; client sends
//! each queued command → server `+QUEUED`; client sends `EXEC` → server
//! returns an array of `N` typed replies, one per queued command. When
//! `WATCH` was issued on the same `Connection` before `MULTI` and any
//! watched key was modified between `WATCH` and `EXEC`, the server
//! returns `Nil` (RESP null array) and the transaction aborts.
//!
//! Embedded mode rejects [`Connection::multi`] / [`Connection::watch`]
//! / [`Connection::unwatch`] with `io::ErrorKind::Unsupported`:
//! kevy-embedded has no MULTI dispatcher, and single-Connection embed
//! access is already sequential (the inner mutex serialises every op),
//! so the locking guarantee transactions add doesn't exist as a
//! separate concept. Call methods directly instead.
//!
//! ```no_run
//! use kevy_client::Connection;
//!
//! let mut conn = Connection::open("kevy://localhost:6379")?;
//! conn.watch(&[b"counter"])?;
//! let mut txn = conn.multi()?;
//! txn.incr(b"counter")?
//!    .set(b"a", b"1")?;
//! match txn.exec_watched()? {
//!     Some(replies) => assert_eq!(replies.len(), 2),
//!     None => { /* watched key changed — retry */ }
//! }
//! # Ok::<(), std::io::Error>(())
//! ```
//!
//! Each queued command's reply is the raw [`kevy_resp::Reply`] — callers
//! parse the typed payload themselves. The reply-side decode (e.g.
//! `let n: i64 = replies[0].as_int()?`) is a v1.6.0 candidate.

use std::io;

use kevy_resp::Reply;
use kevy_resp_client::RespClient;

use crate::{Connection, string, unexpected, vec2, vec3};

/// One in-flight `MULTI` block over a `Remote` connection.
///
/// Drop without calling [`Self::exec`] / [`Self::exec_watched`] /
/// [`Self::discard`] sends an implicit `DISCARD` so the underlying
/// socket isn't left in MULTI mode.
pub struct Transaction<'a> {
    client: &'a mut RespClient,
    /// `false` after `exec`/`exec_watched`/`discard` consumed the txn —
    /// suppresses the implicit-DISCARD in Drop.
    live: bool,
}

impl std::fmt::Debug for Transaction<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Transaction")
            .field("live", &self.live)
            .finish_non_exhaustive()
    }
}

impl Connection {
    /// Start a `MULTI` block. Embedded backend returns
    /// [`io::ErrorKind::Unsupported`].
    pub fn multi(&mut self) -> io::Result<Transaction<'_>> {
        match self {
            Self::Embedded(_) => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "MULTI/EXEC is not implemented for the embedded backend; \
                 call Connection methods directly (each is atomic on its own lock)",
            )),
            Self::Remote(client) => match client.request(&[b"MULTI".to_vec()])? {
                Reply::Simple(s) if s == b"OK" => Ok(Transaction {
                    client,
                    live: true,
                }),
                Reply::Error(e) => Err(io::Error::other(string(e))),
                other => Err(unexpected(other)),
            },
        }
    }

    /// `WATCH key [key ...]` — mark keys for optimistic concurrency.
    /// The next [`multi`](Self::multi) on this connection will abort
    /// (EXEC returns Nil) if any watched key was modified between
    /// this call and EXEC. Remote-only.
    ///
    /// Per RESP spec, WATCH must be sent **before** MULTI. Repeated
    /// `watch` calls accumulate — the abort triggers on any of the
    /// watched keys changing.
    pub fn watch(&mut self, keys: &[&[u8]]) -> io::Result<()> {
        if keys.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "WATCH needs at least one key",
            ));
        }
        match self {
            Self::Embedded(_) => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "WATCH is a transaction primitive; embedded backend has no MULTI",
            )),
            Self::Remote(c) => {
                let mut args = Vec::with_capacity(keys.len() + 1);
                args.push(b"WATCH".to_vec());
                args.extend(keys.iter().map(|k| k.to_vec()));
                match c.request(&args)? {
                    Reply::Simple(s) if s == b"OK" => Ok(()),
                    Reply::Error(e) => Err(io::Error::other(string(e))),
                    other => Err(unexpected(other)),
                }
            }
        }
    }

    /// `UNWATCH` — drop every WATCH set on this connection without
    /// running a transaction. Remote-only.
    pub fn unwatch(&mut self) -> io::Result<()> {
        match self {
            Self::Embedded(_) => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "UNWATCH is a transaction primitive; embedded backend has no MULTI",
            )),
            Self::Remote(c) => match c.request(&[b"UNWATCH".to_vec()])? {
                Reply::Simple(s) if s == b"OK" => Ok(()),
                Reply::Error(e) => Err(io::Error::other(string(e))),
                other => Err(unexpected(other)),
            },
        }
    }
}

impl Transaction<'_> {
    /// Queue one command — verb + args as raw byte slices. The server
    /// replies `+QUEUED` synchronously; errors propagate as `io::Error`.
    pub fn queue(&mut self, parts: &[&[u8]]) -> io::Result<()> {
        if parts.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Transaction::queue needs at least a verb",
            ));
        }
        let argv: Vec<Vec<u8>> = parts.iter().map(|p| p.to_vec()).collect();
        self.queue_argv(argv)
    }

    /// `EXEC` — send EXEC, return the per-queued-command reply array.
    /// Consumes the transaction handle.
    ///
    /// When a `WATCH` violation aborts the transaction the server
    /// returns Nil; this method collapses that into an empty `Vec`
    /// for backward compatibility with v1.4.x. For new code, prefer
    /// [`exec_watched`](Self::exec_watched), which distinguishes
    /// "aborted by WATCH" (returns `None`) from "successful empty
    /// transaction" (returns `Some(vec![])`).
    pub fn exec(mut self) -> io::Result<Vec<Reply>> {
        self.live = false;
        match self.client.request(&[b"EXEC".to_vec()])? {
            Reply::Array(items) => Ok(items),
            Reply::Nil => Ok(Vec::new()),
            Reply::Error(e) => Err(io::Error::other(string(e))),
            other => Err(unexpected(other)),
        }
    }

    /// Like [`exec`](Self::exec) but returns `None` when a `WATCH`
    /// violation aborts the transaction (RESP Nil reply to EXEC).
    /// Use this when you've called [`Connection::watch`] and need to
    /// distinguish an abort from a successfully-empty queue.
    pub fn exec_watched(mut self) -> io::Result<Option<Vec<Reply>>> {
        self.live = false;
        match self.client.request(&[b"EXEC".to_vec()])? {
            Reply::Array(items) => Ok(Some(items)),
            Reply::Nil => Ok(None),
            Reply::Error(e) => Err(io::Error::other(string(e))),
            other => Err(unexpected(other)),
        }
    }

    /// Like [`exec`](Self::exec) but returns a [`TransactionReplies`]
    /// cursor with typed extractors (`next_int`, `next_bulk`, …) so
    /// callers don't hand-match every `Reply` themselves. Aborts with
    /// `io::ErrorKind::InvalidData` ("transaction aborted by WATCH") if
    /// the server replied Nil; use [`exec_watched_typed`](Self::exec_watched_typed)
    /// to distinguish abort from successfully-empty.
    ///
    /// Consumes the handle. The cursor remembers how many replies are
    /// left ([`TransactionReplies::remaining`]) so callers can sanity-
    /// check arity at the end of the read sequence.
    pub fn exec_typed(mut self) -> io::Result<TransactionReplies> {
        self.live = false;
        match self.client.request(&[b"EXEC".to_vec()])? {
            Reply::Array(items) => Ok(TransactionReplies::new(items)),
            Reply::Nil => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "transaction aborted by WATCH",
            )),
            Reply::Error(e) => Err(io::Error::other(string(e))),
            other => Err(unexpected(other)),
        }
    }

    /// Like [`exec_watched`](Self::exec_watched) but returns a typed
    /// [`TransactionReplies`] cursor on commit; `None` on WATCH abort.
    pub fn exec_watched_typed(mut self) -> io::Result<Option<TransactionReplies>> {
        self.live = false;
        match self.client.request(&[b"EXEC".to_vec()])? {
            Reply::Array(items) => Ok(Some(TransactionReplies::new(items))),
            Reply::Nil => Ok(None),
            Reply::Error(e) => Err(io::Error::other(string(e))),
            other => Err(unexpected(other)),
        }
    }

    /// `DISCARD` — abandon the queued commands. Consumes the handle.
    pub fn discard(mut self) -> io::Result<()> {
        self.live = false;
        match self.client.request(&[b"DISCARD".to_vec()])? {
            Reply::Simple(s) if s == b"OK" => Ok(()),
            Reply::Error(e) => Err(io::Error::other(string(e))),
            other => Err(unexpected(other)),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Typed builders (v1.5.0). Each mirrors the same-named Connection method's
// argument shape; on EXEC the matching index in the returned Vec carries
// the typed payload (raw `Reply` — typed decode is a v1.6.0 candidate).
//
// All builders return `&mut Self` so they can chain:
//     txn.set(k, v)?.incr(c)?.del(&[k2])?;
// ─────────────────────────────────────────────────────────────────────────

impl Transaction<'_> {
    /// Queue `SET key value`.
    pub fn set(&mut self, key: &[u8], value: &[u8]) -> io::Result<&mut Self> {
        self.queue_argv(vec3(b"SET", key, value))?;
        Ok(self)
    }

    /// Queue `GET key`.
    pub fn get(&mut self, key: &[u8]) -> io::Result<&mut Self> {
        self.queue_argv(vec2(b"GET", key))?;
        Ok(self)
    }

    /// Queue `DEL key [key ...]`.
    pub fn del(&mut self, keys: &[&[u8]]) -> io::Result<&mut Self> {
        if keys.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Transaction::del needs at least one key",
            ));
        }
        let mut args = Vec::with_capacity(keys.len() + 1);
        args.push(b"DEL".to_vec());
        args.extend(keys.iter().map(|k| k.to_vec()));
        self.queue_argv(args)?;
        Ok(self)
    }

    /// Queue `EXISTS key [key ...]`.
    pub fn exists(&mut self, keys: &[&[u8]]) -> io::Result<&mut Self> {
        if keys.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Transaction::exists needs at least one key",
            ));
        }
        let mut args = Vec::with_capacity(keys.len() + 1);
        args.push(b"EXISTS".to_vec());
        args.extend(keys.iter().map(|k| k.to_vec()));
        self.queue_argv(args)?;
        Ok(self)
    }

    /// Queue `INCR key`.
    pub fn incr(&mut self, key: &[u8]) -> io::Result<&mut Self> {
        self.queue_argv(vec2(b"INCR", key))?;
        Ok(self)
    }

    /// Queue `INCRBY key delta`.
    pub fn incr_by(&mut self, key: &[u8], delta: i64) -> io::Result<&mut Self> {
        let args = vec![
            b"INCRBY".to_vec(),
            key.to_vec(),
            delta.to_string().into_bytes(),
        ];
        self.queue_argv(args)?;
        Ok(self)
    }

    /// Queue `MGET key [key ...]`.
    pub fn mget(&mut self, keys: &[&[u8]]) -> io::Result<&mut Self> {
        if keys.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Transaction::mget needs at least one key",
            ));
        }
        let mut args = Vec::with_capacity(keys.len() + 1);
        args.push(b"MGET".to_vec());
        args.extend(keys.iter().map(|k| k.to_vec()));
        self.queue_argv(args)?;
        Ok(self)
    }

    /// Queue `MSET key value [key value ...]`.
    pub fn mset(&mut self, pairs: &[(&[u8], &[u8])]) -> io::Result<&mut Self> {
        if pairs.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Transaction::mset needs at least one (key, value) pair",
            ));
        }
        let mut args = Vec::with_capacity(pairs.len() * 2 + 1);
        args.push(b"MSET".to_vec());
        for (k, v) in pairs {
            args.push(k.to_vec());
            args.push(v.to_vec());
        }
        self.queue_argv(args)?;
        Ok(self)
    }

    /// Send one already-materialised argv and parse the `+QUEUED` ack.
    /// Shared back-end for `queue` + every typed builder.
    fn queue_argv(&mut self, argv: Vec<Vec<u8>>) -> io::Result<()> {
        match self.client.request(&argv)? {
            Reply::Simple(s) if s == b"QUEUED" => Ok(()),
            Reply::Error(e) => Err(io::Error::other(string(e))),
            other => Err(unexpected(other)),
        }
    }
}

impl Drop for Transaction<'_> {
    fn drop(&mut self) {
        // Implicit DISCARD if the caller dropped the handle without
        // exec/exec_watched/discard. Best-effort: ignore any error
        // since we're in Drop.
        if self.live {
            let _ = self.client.request(&[b"DISCARD".to_vec()]);
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Typed EXEC reply cursor (v1.7.0). Sits between the existing raw
// `Vec<Reply>` API and the maximalist typestate-tuple alternative —
// callers consume queued replies in order via per-typed extractors:
//
//     let mut r = txn.exec_typed()?;
//     let counter: i64       = r.next_int()?;        // INCR
//     let prior:   Option<_> = r.next_bulk()?;       // GET
//     let bulk_m:  Vec<_>    = r.next_array_of_bulks()?;  // MGET
//     r.expect_empty()?;                              // arity gate
//
// Mismatch surfaces InvalidData with the actual variant in the message
// so debugging doesn't require turning on RESP wire logging. The cursor
// also exposes `raw()` as an escape hatch for verbs the typed helpers
// don't cover (HGETALL → array of bulks; ZRANGE WITHSCORES → mixed
// pairs; etc.).
// ─────────────────────────────────────────────────────────────────────────

/// Typed cursor over the per-queued-command replies of a successful
/// `EXEC`. Produced by [`Transaction::exec_typed`] /
/// [`Transaction::exec_watched_typed`]. Each `next_*` consumes one
/// reply; if the variant doesn't match the extractor, an
/// `io::ErrorKind::InvalidData` is returned and the cursor advances
/// regardless (so a downstream `expect_empty` still works correctly).
#[derive(Debug)]
pub struct TransactionReplies {
    iter: std::vec::IntoIter<Reply>,
}

impl TransactionReplies {
    fn new(items: Vec<Reply>) -> Self {
        Self { iter: items.into_iter() }
    }

    /// Number of replies still un-consumed.
    pub fn remaining(&self) -> usize {
        self.iter.len()
    }

    /// Error out if the cursor still has replies — useful at the end of
    /// a typed read sequence to assert the queued-command count matched.
    pub fn expect_empty(&mut self) -> io::Result<()> {
        let left = self.remaining();
        if left == 0 {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("transaction reply cursor has {left} un-consumed replies"),
            ))
        }
    }

    /// Pop the next reply as a raw [`Reply`]. Escape hatch for verbs
    /// the typed extractors don't cover.
    pub fn raw(&mut self) -> io::Result<Reply> {
        self.iter
            .next()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "exhausted"))
    }

    /// Expect `Reply::Simple(b"OK")` — `SET` / `MSET` ack.
    pub fn next_ok(&mut self) -> io::Result<()> {
        match self.raw()? {
            Reply::Simple(s) if s == b"OK" => Ok(()),
            other => Err(mismatch("Simple(OK)", &other)),
        }
    }

    /// Expect `Reply::Simple(b"OK")` OR `Reply::Nil` — `SET key v NX/XX`
    /// returns Nil when the condition is not met.
    pub fn next_ok_or_nil(&mut self) -> io::Result<bool> {
        match self.raw()? {
            Reply::Simple(s) if s == b"OK" => Ok(true),
            Reply::Nil => Ok(false),
            other => Err(mismatch("Simple(OK) or Nil", &other)),
        }
    }

    /// Expect `Reply::Int` — `INCR` / `DEL` / `EXISTS` / `INCRBY`.
    pub fn next_int(&mut self) -> io::Result<i64> {
        match self.raw()? {
            Reply::Int(n) => Ok(n),
            other => Err(mismatch("Int", &other)),
        }
    }

    /// Expect `Reply::Bulk` (or `Nil` → `None`) — `GET`.
    pub fn next_bulk(&mut self) -> io::Result<Option<Vec<u8>>> {
        match self.raw()? {
            Reply::Bulk(b) => Ok(Some(b)),
            Reply::Nil => Ok(None),
            other => Err(mismatch("Bulk or Nil", &other)),
        }
    }

    /// Expect `Reply::Array` of `Bulk`/`Nil` entries — `MGET`. Returns
    /// `Vec<Option<Vec<u8>>>` in request order.
    pub fn next_array_of_bulks(&mut self) -> io::Result<Vec<Option<Vec<u8>>>> {
        let items = match self.raw()? {
            Reply::Array(v) => v,
            Reply::Nil => return Ok(Vec::new()),
            other => return Err(mismatch("Array", &other)),
        };
        items
            .into_iter()
            .map(|r| match r {
                Reply::Bulk(b) => Ok(Some(b)),
                Reply::Nil => Ok(None),
                other => Err(mismatch("Array element Bulk/Nil", &other)),
            })
            .collect()
    }

    /// Expect `Reply::Simple` (any payload) — for verbs whose ack isn't
    /// `OK` (e.g. `PING` → `+PONG`).
    pub fn next_simple(&mut self) -> io::Result<Vec<u8>> {
        match self.raw()? {
            Reply::Simple(s) => Ok(s),
            other => Err(mismatch("Simple", &other)),
        }
    }
}

fn mismatch(want: &str, got: &Reply) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("transaction reply mismatch: expected {want}, got {got:?}"),
    )
}
