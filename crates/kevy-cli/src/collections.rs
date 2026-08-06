//! Reading a family of keys: scan a prefix, read a collection's members.
//!
//! Extracted when the second caller appeared rather than when it looked
//! extractable. `backfill-keys` unions these families; `lint overlap`
//! asks whether they intersect. Same reading, opposite questions.

use std::io;

use kevy_resp_client::{Reply, RespClient};

/// The bulk strings of an array reply, dropping anything else.
pub(crate) fn bulks(reply: Reply) -> Vec<Vec<u8>> {
    let Reply::Array(items) = reply else { return Vec::new() };
    items
        .into_iter()
        .filter_map(|r| match r {
            Reply::Bulk(b) => Some(b),
            _ => None,
        })
        .collect()
}

/// Every key under a prefix, via SCAN so the server is never blocked.
pub(crate) fn scan_prefix(client: &mut RespClient, prefix: &str) -> io::Result<Vec<Vec<u8>>> {
    let pattern = format!("{prefix}*");
    let mut cursor = String::from("0");
    let mut out = Vec::new();
    loop {
        let argv: Vec<&[u8]> =
            vec![b"SCAN", cursor.as_bytes(), b"MATCH", pattern.as_bytes(), b"COUNT", b"500"];
        let Reply::Array(parts) = client.request_borrowed(&argv)? else {
            return Err(io::Error::other("SCAN did not answer with a cursor and a batch"));
        };
        let [Reply::Bulk(next), batch] = parts.as_slice() else {
            return Err(io::Error::other("SCAN reply is not [cursor, keys]"));
        };
        out.extend(bulks(batch.clone()));
        cursor = String::from_utf8_lossy(next).into_owned();
        if cursor == "0" {
            return Ok(out);
        }
    }
}

/// A key's type, as `TYPE` reports it (`none` when it is absent).
pub(crate) fn key_type(client: &mut RespClient, key: &str) -> io::Result<String> {
    Ok(match client.request_borrowed(&[b"TYPE", key.as_bytes()])? {
        Reply::Simple(s) | Reply::Bulk(s) => String::from_utf8_lossy(&s).into_owned(),
        _ => String::new(),
    })
}

/// The verb that reads a collection of that type, if it is one.
///
/// The verb follows the key's type rather than being guessed — a wrong
/// verb answers WRONGTYPE, which would look like an empty collection,
/// and a collection that is silently empty is the failure every caller
/// here exists to prevent.
fn read_verb<'a>(ty: &str, key: &'a [u8]) -> Option<Vec<&'a [u8]>> {
    match ty {
        "set" => Some(vec![b"SMEMBERS", key]),
        "zset" => Some(vec![b"ZRANGE", key, b"0", b"-1"]),
        "list" => Some(vec![b"LRANGE", key, b"0", b"-1"]),
        _ => None,
    }
}

/// The members of a key the caller **named**. Not being a collection is
/// an error: a source that silently contributes nothing is the hole
/// `backfill-keys` exists to close.
pub(crate) fn members(client: &mut RespClient, key: &str) -> io::Result<Vec<Vec<u8>>> {
    let ty = key_type(client, key)?;
    if ty == "none" {
        return Err(io::Error::other(format!("index '{key}' does not exist")));
    }
    let Some(argv) = read_verb(&ty, key.as_bytes()) else {
        return Err(io::Error::other(format!(
            "index '{key}' is a {ty} — a source must be a set, zset, or list of names"
        )));
    };
    Ok(bulks(client.request_borrowed(&argv)?))
}

/// The members of a key the caller **discovered**, or `None` when it is
/// not a collection.
///
/// A prefix scan turns up sidecars — `mailbox:count` beside
/// `mailbox:1` — and failing the whole command because one neighbour is
/// a counter would make it unusable on the first real keyspace. The
/// caller reports how many it skipped, so nothing is dropped quietly.
pub(crate) fn members_if_collection(
    client: &mut RespClient,
    key: &str,
) -> io::Result<Option<Vec<Vec<u8>>>> {
    let ty = key_type(client, key)?;
    let Some(argv) = read_verb(&ty, key.as_bytes()) else { return Ok(None) };
    Ok(Some(bulks(client.request_borrowed(&argv)?)))
}
