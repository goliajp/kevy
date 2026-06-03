//! `MULTI` / `EXEC` / `DISCARD` — Redis transactions.
//!
//! Wire flow (Remote): client sends `MULTI` → server `+OK`; client sends
//! each queued command → server `+QUEUED`; client sends `EXEC` → server
//! returns an array of `N` typed replies, one per queued command.
//!
//! Embedded mode rejects [`Connection::multi`] with
//! `io::ErrorKind::Unsupported`: kevy-embedded has no MULTI dispatcher,
//! and single-Connection embed access is already sequential (the inner
//! mutex serialises every op), so the locking guarantee transactions
//! add doesn't exist as a separate concept. Call methods directly
//! instead.
//!
//! ```no_run
//! use kevy_client::Connection;
//!
//! let mut conn = Connection::open("kevy://localhost:6379")?;
//! let mut txn = conn.multi()?;
//! txn.queue(&[b"SET", b"a", b"1"])?;
//! txn.queue(&[b"INCR", b"counter"])?;
//! let replies = txn.exec()?;
//! assert_eq!(replies.len(), 2);
//! # Ok::<(), std::io::Error>(())
//! ```
//!
//! Each queued command's reply is the raw [`kevy_resp::Reply`] — callers
//! parse the typed payload themselves. v1.4.0 deliberately keeps the
//! `queue(&[verb, args ...])` raw shape; typed builders
//! (`txn.set(k, v)?` → indexed reply on EXEC) are a v1.5.0 candidate.

use std::io;

use kevy_resp::Reply;
use kevy_resp_client::RespClient;

use crate::{Connection, string, unexpected};

/// One in-flight `MULTI` block over a `Remote` connection.
///
/// Drop without calling [`Self::exec`] or [`Self::discard`] sends an
/// implicit `DISCARD` so the underlying socket isn't left in MULTI mode.
pub struct Transaction<'a> {
    client: &'a mut RespClient,
    /// `false` after `exec`/`discard` consumed the txn — suppresses the
    /// implicit-DISCARD in Drop.
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
        match self.client.request(&argv)? {
            Reply::Simple(s) if s == b"QUEUED" => Ok(()),
            Reply::Error(e) => Err(io::Error::other(string(e))),
            other => Err(unexpected(other)),
        }
    }

    /// `EXEC` — send EXEC, return the per-queued-command reply array.
    /// Consumes the transaction handle.
    pub fn exec(mut self) -> io::Result<Vec<Reply>> {
        self.live = false;
        match self.client.request(&[b"EXEC".to_vec()])? {
            Reply::Array(items) => Ok(items),
            // Redis returns a null bulk if EXEC was aborted (WATCH violation, etc.)
            // We don't expose WATCH yet, but stay forward-compatible.
            Reply::Nil => Ok(Vec::new()),
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

impl Drop for Transaction<'_> {
    fn drop(&mut self) {
        // Implicit DISCARD if the caller dropped the handle without
        // exec/discard. Best-effort: ignore any error since we're in Drop.
        if self.live {
            let _ = self.client.request(&[b"DISCARD".to_vec()]);
        }
    }
}
