//! Decode the raw RESP bytes that [`Store::dispatch_argv`] produces into a
//! JSON-friendly reply the webview can consume.
//!
//! The embedded `cmd` path returns the same wire bytes a kevy server would, so
//! we reuse the reference RESP parser (`kevy_resp::parse_reply`) — no bespoke
//! decoder — and then re-shape its [`Reply`] into a serde enum. Byte strings
//! (`Bulk`, `Simple`, …) serialise as JSON number arrays so they stay
//! **binary-safe** across the IPC boundary; the guest binding converts them
//! back to `Uint8Array` (see `guest-js/src/reply.ts`).
//!
//! `dispatch_argv` speaks RESP2 (the embedded store never negotiates `HELLO 3`),
//! so in practice only `Simple`/`Error`/`Int`/`Bulk`/`Nil`/`Array` appear; the
//! remaining RESP3 variants are mapped for completeness.

use kevy_resp::{parse_reply, Reply};
use serde::Serialize;

use crate::error::{Error, Result};

/// A RESP reply re-shaped for JSON transport to the webview.
///
/// Serialises as `{ "type": "<tag>", … }`, mirroring `docs/client-contract.md`
/// §4.1. All byte payloads are JSON arrays of `u8` (binary-safe).
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum JsonReply {
    /// `+OK` — a simple status string.
    Simple {
        /// Status bytes (without the leading `+`).
        bytes: Vec<u8>,
    },
    /// `-ERR …` — an error frame (successful round-trip carrying an error).
    Error {
        /// Error text bytes (without the leading `-`).
        bytes: Vec<u8>,
    },
    /// `:N` — a 64-bit integer.
    Int {
        /// The integer value.
        int: i64,
    },
    /// `$len …` — a bulk (byte) string.
    Bulk {
        /// Payload bytes.
        bytes: Vec<u8>,
    },
    /// `$-1` / `*-1` — the RESP2 null sentinel.
    Nil,
    /// `*N …` — an array of replies.
    Array {
        /// Element replies.
        items: Vec<JsonReply>,
    },
    /// `%N …` (RESP3) — a map of reply pairs.
    Map {
        /// `[key, value]` reply pairs, order preserved.
        pairs: Vec<(JsonReply, JsonReply)>,
    },
    /// `~N …` (RESP3) — a set of replies.
    Set {
        /// Member replies.
        items: Vec<JsonReply>,
    },
    /// `,x` (RESP3) — a double.
    Double {
        /// The double value.
        double: f64,
    },
    /// `#t`/`#f` (RESP3) — a boolean.
    Boolean {
        /// The boolean value.
        boolean: bool,
    },
    /// `=…` (RESP3) — a verbatim string with a 3-char format tag.
    Verbatim {
        /// The 3-char format tag (e.g. `"txt"`, `"mkd"`).
        format: String,
        /// Payload bytes (after the `:` separator).
        bytes: Vec<u8>,
    },
    /// `(…` (RESP3) — an arbitrary-precision integer, carried as digit bytes.
    Bignumber {
        /// Raw digit bytes.
        bytes: Vec<u8>,
    },
    /// `_` (RESP3) — a true null.
    Null,
    /// `>N …` (RESP3) — an out-of-band push frame.
    Push {
        /// Element replies.
        items: Vec<JsonReply>,
    },
    /// `!…` (RESP3) — a length-prefixed blob error.
    Bloberror {
        /// Error bytes.
        bytes: Vec<u8>,
    },
}

impl From<Reply> for JsonReply {
    fn from(r: Reply) -> Self {
        match r {
            Reply::Simple(b) => JsonReply::Simple { bytes: b },
            Reply::Error(b) => JsonReply::Error { bytes: b },
            Reply::Int(n) => JsonReply::Int { int: n },
            Reply::Bulk(b) => JsonReply::Bulk { bytes: b },
            Reply::Nil => JsonReply::Nil,
            Reply::Array(v) => JsonReply::Array { items: map_all(v) },
            Reply::Map(p) => JsonReply::Map { pairs: map_pairs(p) },
            Reply::Set(v) => JsonReply::Set { items: map_all(v) },
            Reply::Double(d) => JsonReply::Double { double: d },
            Reply::Boolean(b) => JsonReply::Boolean { boolean: b },
            Reply::Verbatim { fmt, data } => JsonReply::Verbatim {
                format: String::from_utf8_lossy(&fmt).into_owned(),
                bytes: data,
            },
            Reply::BigNumber(b) => JsonReply::Bignumber { bytes: b },
            Reply::Null => JsonReply::Null,
            Reply::Push(v) => JsonReply::Push { items: map_all(v) },
            Reply::BlobError(b) => JsonReply::Bloberror { bytes: b },
        }
    }
}

fn map_all(v: Vec<Reply>) -> Vec<JsonReply> {
    v.into_iter().map(JsonReply::from).collect()
}

fn map_pairs(p: Vec<(Reply, Reply)>) -> Vec<(JsonReply, JsonReply)> {
    p.into_iter()
        .map(|(k, v)| (JsonReply::from(k), JsonReply::from(v)))
        .collect()
}

/// Parse the single RESP reply `dispatch_argv` wrote into `buf`.
///
/// A malformed or truncated frame is a `Protocol` error (it should never
/// happen — the embedded store emits well-formed RESP — but we surface it
/// honestly rather than panic).
pub fn decode(buf: &[u8]) -> Result<JsonReply> {
    match parse_reply(buf) {
        Ok(Some((reply, _used))) => Ok(reply.into()),
        Ok(None) => Err(Error::Protocol("truncated RESP reply from embedded store".into())),
        Err(e) => Err(Error::Protocol(format!("malformed RESP reply: {e:?}"))),
    }
}

#[cfg(test)]
#[path = "reply_tests.rs"]
mod tests;
