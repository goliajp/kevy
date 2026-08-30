//! Sorted-set algebra: `ZINTERSTORE` / `ZUNIONSTORE` / `ZINTERCARD`.
//!
//! The store forms take the full server option face — `WEIGHTS` and
//! `AGGREGATE SUM|MIN|MAX` ([`ZAggregate`], re-exported from
//! kevy-embedded) — via the `_with` variants; the plain forms are the
//! common unweighted-SUM shorthand.

use crate::{KevyError, KevyResult};

use kevy_embedded::ZAggregate;
use kevy_resp::Reply;
use kevy_resp_client::RespClient;

use crate::{Connection, string, unexpected};

impl Connection {
    /// `ZINTERSTORE dest numkeys key…` — store the intersection of the
    /// given sorted sets into `dest` (unweighted, `AGGREGATE SUM`).
    /// Returns the destination's cardinality.
    pub fn zinterstore(&mut self, dest: &[u8], keys: &[&[u8]]) -> KevyResult<usize> {
        self.zinterstore_with(dest, keys, None, ZAggregate::Sum)
    }

    /// `ZINTERSTORE dest numkeys key… [WEIGHTS w…] [AGGREGATE SUM|MIN|MAX]`
    /// — full option face. `weights`, when given, must have one entry
    /// per key (the backend rejects a mismatch).
    pub fn zinterstore_with(
        &mut self,
        dest: &[u8],
        keys: &[&[u8]],
        weights: Option<&[f64]>,
        aggregate: ZAggregate,
    ) -> KevyResult<usize> {
        check_keys(keys)?;
        match self {
            Self::Embedded(s) => s.zinterstore(dest, keys, weights, aggregate),
            Self::Remote(c) => zstore_request(c, b"ZINTERSTORE", dest, keys, weights, aggregate),
        }
    }

    /// `ZUNIONSTORE dest numkeys key…` — store the union of the given
    /// sorted sets into `dest` (unweighted, `AGGREGATE SUM`). Returns
    /// the destination's cardinality.
    pub fn zunionstore(&mut self, dest: &[u8], keys: &[&[u8]]) -> KevyResult<usize> {
        self.zunionstore_with(dest, keys, None, ZAggregate::Sum)
    }

    /// `ZUNIONSTORE dest numkeys key… [WEIGHTS w…] [AGGREGATE SUM|MIN|MAX]`
    /// — full option face, as in [`Self::zinterstore_with`].
    pub fn zunionstore_with(
        &mut self,
        dest: &[u8],
        keys: &[&[u8]],
        weights: Option<&[f64]>,
        aggregate: ZAggregate,
    ) -> KevyResult<usize> {
        check_keys(keys)?;
        match self {
            Self::Embedded(s) => s.zunionstore(dest, keys, weights, aggregate),
            Self::Remote(c) => zstore_request(c, b"ZUNIONSTORE", dest, keys, weights, aggregate),
        }
    }

    /// `ZINTERCARD numkeys key… [LIMIT n]` — cardinality of the
    /// intersection without materialising it. `limit = None` counts
    /// everything; `Some(n)` short-circuits at `n`.
    pub fn zintercard(&mut self, keys: &[&[u8]], limit: Option<usize>) -> KevyResult<usize> {
        check_keys(keys)?;
        match self {
            Self::Embedded(s) => s.zintercard(keys, limit.unwrap_or(0)),
            Self::Remote(c) => {
                let mut args = Vec::with_capacity(keys.len() + 4);
                args.push(b"ZINTERCARD".to_vec());
                args.push(keys.len().to_string().into_bytes());
                args.extend(keys.iter().map(|k| k.to_vec()));
                if let Some(n) = limit {
                    args.push(b"LIMIT".to_vec());
                    args.push(n.to_string().into_bytes());
                }
                int_reply(c.request(&args)?)
            }
        }
    }
}

/// The verbs require `numkeys ≥ 1` — surface that before the wire.
fn check_keys(keys: &[&[u8]]) -> KevyResult<()> {
    if keys.is_empty() {
        return Err(KevyError::InvalidInput("zset algebra needs at least one source key".into()));
    }
    Ok(())
}

fn aggregate_tag(aggregate: ZAggregate) -> &'static [u8] {
    match aggregate {
        ZAggregate::Sum => b"SUM",
        ZAggregate::Min => b"MIN",
        ZAggregate::Max => b"MAX",
    }
}

/// Build `verb dest numkeys key… [WEIGHTS w…] [AGGREGATE tag]` and
/// parse the cardinality reply. `AGGREGATE` is emitted only for the
/// non-default modes, keeping the wire identical to the plain forms
/// when nothing was customised.
fn zstore_request(
    c: &mut RespClient,
    verb: &[u8],
    dest: &[u8],
    keys: &[&[u8]],
    weights: Option<&[f64]>,
    aggregate: ZAggregate,
) -> KevyResult<usize> {
    let mut args = Vec::with_capacity(keys.len() * 2 + 6);
    args.push(verb.to_vec());
    args.push(dest.to_vec());
    args.push(keys.len().to_string().into_bytes());
    args.extend(keys.iter().map(|k| k.to_vec()));
    if let Some(ws) = weights {
        args.push(b"WEIGHTS".to_vec());
        args.extend(ws.iter().map(|w| format!("{w}").into_bytes()));
    }
    if aggregate != ZAggregate::Sum {
        args.push(b"AGGREGATE".to_vec());
        args.push(aggregate_tag(aggregate).to_vec());
    }
    int_reply(c.request(&args)?)
}

fn int_reply(reply: Reply) -> KevyResult<usize> {
    match reply {
        Reply::Int(n) if n >= 0 => Ok(n as usize),
        Reply::Error(e) => Err(KevyError::Protocol(string(e))),
        other => Err(unexpected(other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed(c: &mut Connection) {
        c.zadd(b"za", &[(1.0, b"x".as_ref()), (2.0, b"y".as_ref())]).unwrap();
        c.zadd(b"zb", &[(10.0, b"y".as_ref()), (20.0, b"z".as_ref())]).unwrap();
    }

    #[test]
    fn embedded_zinterstore_and_zunionstore() {
        let mut c = Connection::connect("mem://").unwrap();
        seed(&mut c);
        assert_eq!(c.zinterstore(b"zi", &[&b"za"[..], &b"zb"[..]]).unwrap(), 1);
        assert_eq!(c.zscore(b"zi", b"y").unwrap(), Some(12.0)); // SUM
        assert_eq!(c.zunionstore(b"zu", &[&b"za"[..], &b"zb"[..]]).unwrap(), 3);
    }

    #[test]
    fn embedded_zstore_with_weights_and_aggregate() {
        let mut c = Connection::connect("mem://").unwrap();
        seed(&mut c);
        let n = c
            .zunionstore_with(b"zw", &[&b"za"[..], &b"zb"[..]], Some(&[2.0, 1.0]), ZAggregate::Max)
            .unwrap();
        assert_eq!(n, 3);
        // y: max(2*2, 10*1) = 10
        assert_eq!(c.zscore(b"zw", b"y").unwrap(), Some(10.0));
    }

    #[test]
    fn embedded_zintercard_with_and_without_limit() {
        let mut c = Connection::connect("mem://").unwrap();
        c.zadd(b"za", &[(1.0, b"a".as_ref()), (2.0, b"b".as_ref()), (3.0, b"c".as_ref())]).unwrap();
        c.zadd(b"zb", &[(1.0, b"a".as_ref()), (2.0, b"b".as_ref()), (9.0, b"q".as_ref())]).unwrap();
        assert_eq!(c.zintercard(&[&b"za"[..], &b"zb"[..]], None).unwrap(), 2);
        assert_eq!(c.zintercard(&[&b"za"[..], &b"zb"[..]], Some(1)).unwrap(), 1);
    }

    #[test]
    fn empty_keys_rejected() {
        let mut c = Connection::connect("mem://").unwrap();
        let err = c.zintercard(&[], None).unwrap_err();
        assert!(matches!(err, KevyError::InvalidInput(_)));
    }
}
