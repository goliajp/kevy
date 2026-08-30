//! Pipelined batches: queue N commands client-side, send
//! them as one write, read N replies in order.
//!
//! Unlike [`Connection::multi`](crate::Connection::multi) this is
//! **not** atomic — the server may interleave other clients' commands
//! between the batch's — it only removes the per-command round-trip.
//! Remote-only: the embedded backend has no argv dispatcher (same
//! rationale as MULTI) and its calls are in-process already; it
//! answers `Unsupported`.
//!
//! ```no_run
//! use kevy_client::Connection;
//!
//! let mut conn = Connection::connect("kevy://localhost:6379")?;
//! let replies = conn.pipeline(|p| {
//!     p.cmd(&[b"SET", b"a", b"1"]).cmd(&[b"INCR", b"n"]).cmd(&[b"GET", b"a"]);
//! })?;
//! assert_eq!(replies.len(), 3);
//! # Ok::<(), kevy_client::KevyError>(())
//! ```

use crate::{KevyError, KevyResult};

use kevy_resp::{Reply, encode_command_borrowed};

use crate::Connection;

/// Client-side command buffer for [`Connection::pipeline`]. Each
/// [`Self::cmd`] appends one RESP-encoded command; the whole buffer is
/// flushed in a single write.
#[derive(Debug, Default)]
pub struct PipelineBuf {
    buf: Vec<u8>,
    count: usize,
    /// Set when a `cmd` call was malformed (empty argv) — surfaced as
    /// `InvalidInput` at send time so the builder closure can stay
    /// infallible-chainable.
    poisoned: bool,
}

impl PipelineBuf {
    /// Queue one command — verb + args as raw byte slices, e.g.
    /// `p.cmd(&[b"SET", key, value])`. Chainable. An empty `parts`
    /// poisons the buffer and [`Connection::pipeline`] reports it.
    pub fn cmd(&mut self, parts: &[&[u8]]) -> &mut Self {
        if parts.is_empty() {
            self.poisoned = true;
            return self;
        }
        encode_command_borrowed(&mut self.buf, parts);
        self.count += 1;
        self
    }

    /// Number of queued commands.
    pub fn len(&self) -> usize {
        self.count
    }

    /// Whether nothing has been queued.
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }
}

impl Connection {
    /// Run a pipelined batch: `build` queues commands into a
    /// [`PipelineBuf`], then the batch is sent as one write and the
    /// per-command replies are returned **in queue order**.
    ///
    /// Error replies to individual commands come back as
    /// [`Reply::Error`] entries (they don't abort the batch) — check
    /// each reply, unlike the typed single-command wraps which map
    /// server errors to `io::Error`. An empty batch returns `Ok(vec![])`
    /// without touching the wire.
    pub fn pipeline(&mut self, build: impl FnOnce(&mut PipelineBuf)) -> KevyResult<Vec<Reply>> {
        let client = self.remote("pipeline")?;
        let mut p = PipelineBuf::default();
        build(&mut p);
        if p.poisoned {
            return Err(KevyError::InvalidInput(
                "pipeline: a queued command had an empty argv".into(),
            ));
        }
        if p.count == 0 {
            return Ok(Vec::new());
        }
        Ok(client.pipeline_raw(&p.buf, p.count)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_pipeline_unsupported() {
        let mut c = Connection::connect("mem://").unwrap();
        let err = c.pipeline(|p| {
            p.cmd(&[b"PING"]);
        });
        assert!(matches!(err.unwrap_err(), KevyError::Unsupported(_)));
    }

    #[test]
    fn pipeline_buf_counts_and_encodes() {
        let mut p = PipelineBuf::default();
        assert!(p.is_empty());
        p.cmd(&[b"SET", b"a", b"1"]).cmd(&[b"GET", b"a"]);
        assert_eq!(p.len(), 2);
        assert!(p.buf.starts_with(b"*3\r\n$3\r\nSET\r\n"));
    }
}
