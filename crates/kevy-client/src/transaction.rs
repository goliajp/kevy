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

impl<'a> Transaction<'a> {
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

impl<'a> Transaction<'a> {
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
