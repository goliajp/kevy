//! `status`: what the connection reaches — server, role, size, and the
//! relational catalogs — one field per row.

use super::options::Common;
use super::render::render;
use super::rows::{Cell, Rows};
use crate::rcli::send::write_out;
use crate::rcli::session::Session;
use kevy_resp::Reply;

/// Run `status`; the exit code.
pub(crate) fn run(s: &mut Session, common: &Common) -> u8 {
    let mut fields: Vec<(&str, Cell)> = Vec::new();
    let hello = s.request(&[b"HELLO"]).ok();
    for name in ["server", "version", "mode", "role"] {
        fields.push((
            name,
            hello.as_ref().and_then(|h| entry(h, name.as_bytes())).unwrap_or(Cell::Null),
        ));
    }
    let target = match &s.opts.socket {
        Some(path) => path.clone(),
        None => [s.opts.host.as_slice(), format!(":{}", s.opts.port).as_bytes()].concat(),
    };
    fields.push(("address", Cell::Text(target)));
    fields.push(("keys", number(s, &[b"DBSIZE"])));
    fields.push(("shards", number(s, &[b"FEED.SHARDS"])));
    let feed = match s.request(&[b"FEED.TAIL", b"0"]) {
        Ok(Reply::Error(_)) | Err(_) => b"off".to_vec(),
        Ok(_) => b"on".to_vec(),
    };
    fields.push(("feed", Cell::Text(feed)));
    for (name, verb) in
        [("tables", &b"TABLE.LIST"[..]), ("indexes", b"IDX.LIST"), ("views", b"VIEW.LIST")]
    {
        let count = match s.request(&[verb]) {
            Ok(Reply::Array(items)) => Cell::Int(items.len() as i64),
            _ => Cell::Null,
        };
        fields.push((name, count));
    }
    let rows = Rows {
        columns: vec![b"field".to_vec(), b"value".to_vec()],
        rows: fields.into_iter().map(|(n, v)| vec![Cell::Text(n.as_bytes().to_vec()), v]).collect(),
    };
    write_out(&render(&rows, &common.style));
    0
}

/// A field of HELLO's reply (a map, or flat pairs in RESP2).
fn entry(hello: &Reply, name: &[u8]) -> Option<Cell> {
    let value = match hello {
        Reply::Map(m) => m
            .iter()
            .find(|(k, _)| matches!(k, Reply::Bulk(b) | Reply::Simple(b) if b == name))
            .map(|(_, v)| v),
        Reply::Array(flat) => flat
            .chunks(2)
            .find(|kv| matches!(&kv[0], Reply::Bulk(b) | Reply::Simple(b) if b == name))
            .and_then(|kv| kv.get(1)),
        _ => None,
    }?;
    match value {
        Reply::Bulk(b) | Reply::Simple(b) => Some(Cell::Text(b.clone())),
        Reply::Int(n) => Some(Cell::Int(*n)),
        _ => None,
    }
}

fn number(s: &mut Session, argv: &[&[u8]]) -> Cell {
    match s.request(argv) {
        Ok(Reply::Int(n)) => Cell::Int(n),
        _ => Cell::Null,
    }
}
