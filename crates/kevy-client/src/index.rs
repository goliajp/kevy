//! Declarative secondary indexes: `IDX.CREATE` / `IDX.QUERY` /
//! `IDX.DROP` / `IDX.LIST`.
//!
//! **Remote-only.** The embedded backend answers `Unsupported`: the
//! wire face coerces query bounds through the index's declared value
//! type server-side, which the client cannot replicate without the
//! catalog. Embedded users match the public
//! [`Connection::Embedded`](crate::Connection) variant and call
//! [`kevy_embedded::Store`]'s typed `idx_*` API directly.
//!
//! Shape strategy: `IDX.CREATE` / `IDX.QUERY` have a large argument
//! face (see docs/verb-reference.md), so the wrap is two-layered —
//! typed shortcuts for the common forms (`RANGE` / `EQ` / `MATCH` /
//! `KNN`, plain range-index create) plus `*_raw` argv passthroughs
//! that keep every server capability reachable (COMPOSE, HYBRID,
//! GROUPS, ANN create options, …) without this crate chasing the verb
//! grammar release-by-release.

use crate::{KevyError, KevyResult};

use kevy_resp::Reply;

use crate::{Connection, num_f64, num_u64, string, unexpected};

/// Declared scalar type for [`Connection::idx_create_range`]
/// (`TYPE i64|f64|str`). Vector/ANN indexes have extra required
/// options — declare those via [`Connection::idx_create_raw`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdxType {
    /// `TYPE i64` — signed 64-bit integer field.
    I64,
    /// `TYPE f64` — finite 64-bit float field.
    F64,
    /// `TYPE str` — raw bytes, memcmp order.
    Str,
}

impl IdxType {
    fn tag(self) -> &'static [u8] {
        match self {
            Self::I64 => b"i64",
            Self::F64 => b"f64",
            Self::Str => b"str",
        }
    }
}

/// One `IDX.QUERY` hit: the row's key plus the indexed value's string
/// form (the same repr the wire carries).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdxRow {
    /// The matching key.
    pub key: Vec<u8>,
    /// The indexed field value, in its wire string form.
    pub value: Vec<u8>,
}

/// One page of `IDX.QUERY RANGE`/`EQ` results.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdxPage {
    /// Cursor for the next page — `None` when the scan is complete;
    /// otherwise pass it back via `idx_query_range`'s `cursor`.
    pub cursor: Option<Vec<u8>>,
    /// The page's hits in `(value, key)` order.
    pub rows: Vec<IdxRow>,
}

/// One declared index, as reported by `IDX.LIST`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdxInfo {
    /// Index name.
    pub name: Vec<u8>,
    /// Key prefix the index covers.
    pub prefix: Vec<u8>,
    /// Kind tag (`range` / `unique` / `text` / `ann` / `agg`).
    pub kind: String,
    /// Build state (`ready` / `building`).
    pub state: String,
    /// Total indexed entries across shards.
    pub entries: u64,
    /// Total index bytes across shards.
    pub bytes: u64,
}

impl Connection {
    /// `IDX.CREATE name ON PREFIX prefix FIELD field TYPE ty KIND range`
    /// — declare a plain range index (the common case: range + EQ
    /// queries over one hash field under a key prefix).
    pub fn idx_create_range(
        &mut self,
        name: &[u8],
        prefix: &[u8],
        field: &[u8],
        ty: IdxType,
    ) -> KevyResult<()> {
        let args: &[&[u8]] = &[
            name, b"ON", b"PREFIX", prefix, b"FIELD", field, b"TYPE", ty.tag(), b"KIND",
            b"range",
        ];
        self.idx_create_raw(args)
    }

    /// `IDX.CREATE <args…>` — raw passthrough; `args` is everything
    /// after the verb (see docs/verb-reference.md for the full
    /// grammar: MAXMEM, DIM/DISTANCE/M/EF for ANN, GROUPBY for agg…).
    pub fn idx_create_raw(&mut self, args: &[&[u8]]) -> KevyResult<()> {
        match self.remote("IDX.CREATE")?.request(&raw_argv(b"IDX.CREATE", args))? {
            Reply::Simple(s) if s == b"OK" => Ok(()),
            Reply::Error(e) => Err(KevyError::Protocol(string(e))),
            other => Err(unexpected(other)),
        }
    }

    /// `IDX.DROP name` — returns whether the index existed.
    pub fn idx_drop(&mut self, name: &[u8]) -> KevyResult<bool> {
        match self.remote("IDX.DROP")?.request_borrowed(&[b"IDX.DROP", name])? {
            Reply::Int(1) => Ok(true),
            Reply::Int(0) => Ok(false),
            Reply::Error(e) => Err(KevyError::Protocol(string(e))),
            other => Err(unexpected(other)),
        }
    }

    /// `IDX.LIST` — declared indexes with build state and stats.
    pub fn idx_list(&mut self) -> KevyResult<Vec<IdxInfo>> {
        match self.remote("IDX.LIST")?.request_borrowed(&[b"IDX.LIST"])? {
            Reply::Array(items) => items.into_iter().map(parse_info).collect(),
            Reply::Error(e) => Err(KevyError::Protocol(string(e))),
            other => Err(unexpected(other)),
        }
    }

    /// `IDX.QUERY name RANGE min max [LIMIT n] [CURSOR c]` — one page
    /// of `(key, value)` hits in `(value, key)` order. `min`/`max` are
    /// the wire string forms (e.g. `b"18"`), coerced server-side per
    /// the index's declared type. Resume with [`IdxPage::cursor`].
    pub fn idx_query_range(
        &mut self,
        name: &[u8],
        min: &[u8],
        max: &[u8],
        limit: usize,
        cursor: Option<&[u8]>,
    ) -> KevyResult<IdxPage> {
        let lim = limit.to_string();
        let mut args: Vec<&[u8]> = vec![name, b"RANGE", min, max, b"LIMIT", lim.as_bytes()];
        if let Some(c) = cursor {
            args.push(b"CURSOR");
            args.push(c);
        }
        parse_page(self.idx_query_raw(&args)?)
    }

    /// `IDX.QUERY name EQ value [LIMIT n]` — point-lookup page.
    pub fn idx_query_eq(&mut self, name: &[u8], value: &[u8], limit: usize) -> KevyResult<IdxPage> {
        let lim = limit.to_string();
        parse_page(self.idx_query_raw(&[name, b"EQ", value, b"LIMIT", lim.as_bytes()])?)
    }

    /// `IDX.QUERY name MATCH text [LIMIT n]` — BM25-ranked full-text
    /// hits as `(key, score)`, best first (needs a `KIND text` index).
    pub fn idx_query_match(
        &mut self,
        name: &[u8],
        text: &[u8],
        limit: usize,
    ) -> KevyResult<Vec<(Vec<u8>, f64)>> {
        let lim = limit.to_string();
        parse_ranked(self.idx_query_raw(&[name, b"MATCH", text, b"LIMIT", lim.as_bytes()])?)
    }

    /// `IDX.QUERY name KNN vector [LIMIT k]` — nearest neighbours as
    /// `(key, distance)`, closest first (needs a `KIND ann` index).
    /// `vector` is encoded as the f32 little-endian blob the index
    /// stores.
    pub fn idx_query_knn(
        &mut self,
        name: &[u8],
        vector: &[f32],
        k: usize,
    ) -> KevyResult<Vec<(Vec<u8>, f64)>> {
        let blob: Vec<u8> = vector.iter().flat_map(|f| f.to_le_bytes()).collect();
        let lim = k.to_string();
        parse_ranked(self.idx_query_raw(&[name, b"KNN", &blob, b"LIMIT", lim.as_bytes()])?)
    }

    /// `IDX.QUERY <args…>` — raw passthrough returning the raw
    /// [`Reply`]; `args` is everything after the verb. The escape
    /// hatch for COMPOSE / HYBRID / GROUPS / FIELDS hydration and any
    /// future query shape.
    pub fn idx_query_raw(&mut self, args: &[&[u8]]) -> KevyResult<Reply> {
        match self.remote("IDX.QUERY")?.request(&raw_argv(b"IDX.QUERY", args))? {
            Reply::Error(e) => Err(KevyError::Protocol(string(e))),
            other => Ok(other),
        }
    }
}

fn raw_argv(verb: &[u8], args: &[&[u8]]) -> Vec<Vec<u8>> {
    let mut argv = Vec::with_capacity(args.len() + 1);
    argv.push(verb.to_vec());
    argv.extend(args.iter().map(|a| a.to_vec()));
    argv
}

/// `*2 [cursor, *2N [key, value]…]` → [`IdxPage`]. Cursor `0` = done.
fn parse_page(reply: Reply) -> KevyResult<IdxPage> {
    let Reply::Array(items) = reply else {
        return Err(unexpected(reply));
    };
    if items.len() != 2 {
        return Err(KevyError::Protocol("IDX.QUERY page: expected [cursor, rows]".into()));
    }
    let mut it = items.into_iter();
    let cursor = match it.next().unwrap() {
        Reply::Bulk(c) if c == b"0" => None,
        Reply::Bulk(c) => Some(c),
        other => return Err(unexpected(other)),
    };
    let Reply::Array(flat) = it.next().unwrap() else {
        return Err(KevyError::Protocol("IDX.QUERY page: rows not an array".into()));
    };
    let mut rows = Vec::with_capacity(flat.len() / 2);
    let mut flat = flat.into_iter();
    while let Some(k) = flat.next() {
        let (Reply::Bulk(key), Some(Reply::Bulk(value))) = (k, flat.next()) else {
            return Err(KevyError::Protocol("IDX.QUERY page: odd or non-bulk row pair".into()));
        };
        rows.push(IdxRow { key, value });
    }
    Ok(IdxPage { cursor, rows })
}

/// MATCH/KNN shape: `*N` rows of `*(2+2F) [key, score, fields…]` →
/// `(key, score)` (shortcuts request no FIELDS, so F = 0).
fn parse_ranked(reply: Reply) -> KevyResult<Vec<(Vec<u8>, f64)>> {
    let Reply::Array(items) = reply else {
        return Err(unexpected(reply));
    };
    items
        .into_iter()
        .map(|row| {
            let Reply::Array(cells) = row else {
                return Err(unexpected(row));
            };
            let mut it = cells.into_iter();
            match (it.next(), it.next()) {
                (Some(Reply::Bulk(key)), Some(Reply::Bulk(score))) => {
                    Ok((key, num_f64(&score)?))
                }
                _ => Err(KevyError::Protocol("IDX.QUERY ranked row: expected [key, score]".into())),
            }
        })
        .collect()
}

/// One `IDX.LIST` entry: a flat label/value bulk array
/// (`name … prefix … kind … state … entries … bytes …`).
fn parse_info(entry: Reply) -> KevyResult<IdxInfo> {
    let Reply::Array(cells) = entry else {
        return Err(unexpected(entry));
    };
    let mut info = IdxInfo {
        name: Vec::new(),
        prefix: Vec::new(),
        kind: String::new(),
        state: String::new(),
        entries: 0,
        bytes: 0,
    };
    let mut it = cells.into_iter();
    while let (Some(Reply::Bulk(label)), Some(Reply::Bulk(value))) = (it.next(), it.next()) {
        match label.as_slice() {
            b"name" => info.name = value,
            b"prefix" => info.prefix = value,
            b"kind" => info.kind = string(value),
            b"state" => info.state = string(value),
            b"entries" => info.entries = num_u64(&value)?,
            b"bytes" => info.bytes = num_u64(&value)?,
            _ => {} // forward-compatible: skip labels this version doesn't know
        }
    }
    Ok(info)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_idx_is_unsupported() {
        let mut c = Connection::connect("mem://").unwrap();
        let err = c.idx_list().unwrap_err();
        assert!(matches!(err, KevyError::Unsupported(_)));
        let err = c
            .idx_create_range(b"i", b"user:", b"age", IdxType::I64)
            .unwrap_err();
        assert!(matches!(err, KevyError::Unsupported(_)));
    }

    #[test]
    fn page_parser_maps_cursor_and_rows() {
        let reply = Reply::Array(vec![
            Reply::Bulk(b"abc1".to_vec()),
            Reply::Array(vec![
                Reply::Bulk(b"user:1".to_vec()),
                Reply::Bulk(b"21".to_vec()),
                Reply::Bulk(b"user:2".to_vec()),
                Reply::Bulk(b"22".to_vec()),
            ]),
        ]);
        let page = parse_page(reply).unwrap();
        assert_eq!(page.cursor, Some(b"abc1".to_vec()));
        assert_eq!(page.rows.len(), 2);
        assert_eq!(page.rows[0].key, b"user:1");
        assert_eq!(page.rows[0].value, b"21");

        let done = parse_page(Reply::Array(vec![
            Reply::Bulk(b"0".to_vec()),
            Reply::Array(vec![]),
        ]))
        .unwrap();
        assert_eq!(done.cursor, None);
        assert!(done.rows.is_empty());
    }

    #[test]
    fn ranked_parser_maps_key_score() {
        let reply = Reply::Array(vec![Reply::Array(vec![
            Reply::Bulk(b"doc:1".to_vec()),
            Reply::Bulk(b"1.5".to_vec()),
        ])]);
        assert_eq!(parse_ranked(reply).unwrap(), vec![(b"doc:1".to_vec(), 1.5)]);
    }
}
