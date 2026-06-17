//! Crate-internal RESP/argv conversion helpers, used by [`crate`] +
//! `collections.rs` + `subscribe.rs`. Split out of `lib.rs` for file-size
//! hygiene; nothing here is part of the public surface.

use std::io;

use kevy_resp::Reply;

pub(crate) fn vec2(verb: &[u8], a: &[u8]) -> Vec<Vec<u8>> {
    vec![verb.to_vec(), a.to_vec()]
}

pub(crate) fn vec3(verb: &[u8], a: &[u8], b: &[u8]) -> Vec<Vec<u8>> {
    vec![verb.to_vec(), a.to_vec(), b.to_vec()]
}

pub(crate) fn string(b: Vec<u8>) -> String {
    String::from_utf8_lossy(&b).into_owned()
}

pub(crate) fn unexpected(r: Reply) -> io::Error {
    let kind = match r {
        Reply::Simple(_) => "simple-string",
        Reply::Error(_) => "error",
        Reply::Int(_) => "integer",
        Reply::Bulk(_) => "bulk-string",
        Reply::Nil | Reply::Null => "nil",
        Reply::Array(_) => "array",
        Reply::Map(_) => "map",
        Reply::Set(_) => "set",
        Reply::Double(_) => "double",
        Reply::Boolean(_) => "boolean",
        Reply::Verbatim { .. } => "verbatim-string",
        Reply::BigNumber(_) => "big-number",
        Reply::Push(_) => "push",
        Reply::BlobError(_) => "blob-error",
    };
    io::Error::other(format!("unexpected RESP reply variant: {kind}"))
}

pub(crate) fn array_to_bulks(items: Vec<Reply>) -> io::Result<Vec<Vec<u8>>> {
    items
        .into_iter()
        .map(|r| match r {
            Reply::Bulk(v) | Reply::Simple(v) => Ok(v),
            Reply::Nil => Ok(Vec::new()),
            other => Err(unexpected(other)),
        })
        .collect()
}

pub(crate) fn store_err(e: kevy_embedded::StoreError) -> io::Error {
    io::Error::other(format!("kevy-store: {e:?}"))
}
