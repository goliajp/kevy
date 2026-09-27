//! Replies of the relational verbs, read into columns and rows.

use kevy_resp::Reply;

/// One cell: text, a number, or nothing (a missing field — NULL).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Cell {
    Text(Vec<u8>),
    Int(i64),
    Null,
}

/// A result set: column names and rows of cells, each row as long as the
/// columns.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Rows {
    pub(crate) columns: Vec<Vec<u8>>,
    pub(crate) rows: Vec<Vec<Cell>>,
}

impl Rows {
    fn column(&mut self, name: &[u8]) -> usize {
        if let Some(i) = self.columns.iter().position(|c| c == name) {
            return i;
        }
        self.columns.push(name.to_vec());
        for row in &mut self.rows {
            row.push(Cell::Null);
        }
        self.columns.len() - 1
    }

    fn push(&mut self, pairs: &[(&[u8], Cell)]) {
        let indexes: Vec<usize> = pairs.iter().map(|(name, _)| self.column(name)).collect();
        let mut row = vec![Cell::Null; self.columns.len()];
        for (i, (_, cell)) in indexes.into_iter().zip(pairs) {
            row[i] = cell.clone();
        }
        self.rows.push(row);
    }
}

fn cell(reply: &Reply) -> Cell {
    match reply {
        Reply::Bulk(b) | Reply::Simple(b) | Reply::Verbatim { data: b, .. } => {
            Cell::Text(b.clone())
        }
        Reply::Int(n) => Cell::Int(*n),
        Reply::Double(d) => Cell::Text(d.to_string().into_bytes()),
        Reply::Boolean(t) => Cell::Int(i64::from(*t)),
        _ => Cell::Null,
    }
}

fn items(reply: &Reply) -> Option<&[Reply]> {
    match reply {
        Reply::Array(v) | Reply::Set(v) | Reply::Push(v) => Some(v),
        _ => None,
    }
}

fn text(reply: &Reply) -> Option<&[u8]> {
    match reply {
        Reply::Bulk(b) | Reply::Simple(b) | Reply::Verbatim { data: b, .. } => Some(b),
        _ => None,
    }
}

/// Key/value pairs, flat (`[k, v, k, v]`) or a map.
fn pairs(reply: &Reply) -> Option<Vec<(&[u8], Cell)>> {
    match reply {
        Reply::Map(m) => m.iter().map(|(k, v)| Some((text(k)?, cell(v)))).collect(),
        other => {
            let flat = items(other)?;
            if flat.len() % 2 != 0 {
                return None;
            }
            flat.chunks(2).map(|kv| Some((text(&kv[0])?, cell(&kv[1])))).collect()
        }
    }
}

/// `TABLE.LIST`, `IDX.LIST`, `VIEW.LIST`, `*.VERIFY`: one set of pairs per
/// row; a row's new keys become new columns.
pub(crate) fn from_pair_rows(reply: &Reply) -> Option<Rows> {
    let mut rows = Rows::default();
    for row in items(reply)? {
        rows.push(&pairs(row)?);
    }
    Some(rows)
}

/// `IDX.EXPLAIN`: `[[name, value], …]` or flat pairs, as a single row.
pub(crate) fn from_pair_list(reply: &Reply) -> Option<Rows> {
    let list = items(reply)?;
    let nested: Option<Vec<(&[u8], Cell)>> = list
        .iter()
        .map(|p| match items(p)? {
            [k, v] => Some((text(k)?, cell(v))),
            _ => None,
        })
        .collect();
    let mut rows = Rows::default();
    rows.push(&nested.or_else(|| pairs(reply))?);
    Some(rows)
}

/// `IDX.QUERY` / `VIEW.QUERY`: the cursor, and the page as rows —
/// `key, value` flat pairs, or `[key, value, field, value, …]` per row.
pub(crate) fn from_query(reply: &Reply, value_column: &[u8]) -> Option<(Vec<u8>, Rows)> {
    let top = items(reply)?;
    let cursor = text(top.first()?)?.to_vec();
    let body: Vec<&Reply> = match top.get(1..)? {
        [Reply::Array(page)] => page.iter().collect(),
        rest => rest.iter().collect(),
    };
    let mut rows = Rows { columns: vec![b"key".to_vec(), value_column.to_vec()], rows: Vec::new() };
    if body.iter().all(|r| items(r).is_some()) && !body.is_empty() {
        for row in body {
            let fields = items(row)?;
            let (key, value) = (fields.first()?, fields.get(1)?);
            let mut named: Vec<(&[u8], Cell)> =
                vec![(b"key", cell(key)), (value_column, cell(value))];
            for kv in fields.get(2..)?.chunks(2) {
                if let [k, v] = kv {
                    named.push((text(k)?, cell(v)));
                }
            }
            rows.push(&named);
        }
    } else {
        for kv in body.chunks(2) {
            if let [k, v] = kv {
                rows.push(&[(&b"key"[..], cell(k)), (value_column, cell(v))]);
            }
        }
    }
    Some((cursor, rows))
}

/// `IDX.ADVISE`: `[hits, name, command]` per row.
pub(crate) fn from_advise(reply: &Reply) -> Option<Rows> {
    let mut rows = Rows {
        columns: vec![b"hits".to_vec(), b"name".to_vec(), b"command".to_vec()],
        rows: Vec::new(),
    };
    for row in items(reply)? {
        match items(row)? {
            [h, n, c] => rows.rows.push(vec![cell(h), cell(n), cell(c)]),
            _ => return None,
        }
    }
    Some(rows)
}

#[cfg(test)]
mod tests {
    use super::{Cell, Rows, from_advise, from_pair_list, from_pair_rows, from_query};
    use kevy_resp::Reply;

    fn b(s: &str) -> Reply {
        Reply::Bulk(s.as_bytes().to_vec())
    }
    fn arr(v: Vec<Reply>) -> Reply {
        Reply::Array(v)
    }
    fn t(s: &str) -> Cell {
        Cell::Text(s.as_bytes().to_vec())
    }
    fn cols(r: &Rows) -> Vec<String> {
        r.columns.iter().map(|c| String::from_utf8_lossy(c).into_owned()).collect()
    }

    #[test]
    fn list_rows_take_their_columns_from_the_keys() {
        let reply = arr(vec![
            arr(vec![b("name"), b("users"), b("pk"), b("id")]),
            arr(vec![b("name"), b("orders"), b("pk"), b("id"), b("window"), b("ts")]),
        ]);
        let rows = from_pair_rows(&reply).unwrap();
        assert_eq!(cols(&rows), ["name", "pk", "window"]);
        assert_eq!(rows.rows[0], [t("users"), t("id"), Cell::Null]);
        assert_eq!(rows.rows[1][2], t("ts"));
        assert!(from_pair_rows(&arr(vec![arr(vec![b("odd")])])).is_none());
    }

    #[test]
    fn query_pages_read_flat_or_with_fields() {
        let flat = arr(vec![b("0"), arr(vec![b("user:1"), b("21"), b("user:2"), b("22")])]);
        let (cursor, rows) = from_query(&flat, b"value").unwrap();
        assert_eq!(
            (cursor.as_slice(), cols(&rows)),
            (&b"0"[..], vec!["key".to_string(), "value".into()])
        );
        assert_eq!(rows.rows.len(), 2);
        let fields = arr(vec![
            b("c1"),
            arr(vec![arr(vec![b("user:1"), b("21"), b("name"), b("n1"), b("age"), Reply::Nil])]),
        ]);
        let (cursor, rows) = from_query(&fields, b"value").unwrap();
        assert_eq!(
            (cursor.as_slice(), cols(&rows)),
            (&b"c1"[..], vec!["key".into(), "value".into(), "name".into(), "age".into()])
        );
        assert_eq!(rows.rows[0][3], Cell::Null);
        let view = arr(vec![b("0"), b("k1"), b("5"), b("k2"), b("6")]);
        let (_, rows) = from_query(&view, b"order_value").unwrap();
        assert_eq!((rows.rows.len(), rows.rows[1][1].clone()), (2, t("6")));
    }

    #[test]
    fn resp3_replies_read_like_their_resp2_twins() {
        let map = Reply::Set(vec![Reply::Map(vec![
            (
                Reply::Simple(b"name".to_vec()),
                Reply::Verbatim { fmt: *b"txt", data: b"u".to_vec() },
            ),
            (b("ratio"), Reply::Double(0.5)),
            (b("ready"), Reply::Boolean(true)),
            (b("gone"), Reply::Nil),
        ])]);
        let rows = from_pair_rows(&map).unwrap();
        assert_eq!(cols(&rows), ["name", "ratio", "ready", "gone"]);
        assert_eq!(rows.rows[0], [t("u"), t("0.5"), Cell::Int(1), Cell::Null]);
        let keyless = Reply::Array(vec![Reply::Map(vec![(Reply::Int(1), b("x"))])]);
        assert!(from_pair_rows(&keyless).is_none(), "a key must be text");
        let pushed = Reply::Push(vec![arr(vec![b("kind"), b("range")])]);
        assert_eq!(from_pair_rows(&pushed).unwrap().rows.len(), 1);
    }

    #[test]
    fn replies_of_another_shape_are_refused_not_guessed() {
        assert!(from_pair_rows(&b("OK")).is_none());
        assert!(from_pair_list(&b("OK")).is_none());
        let flat_explain = arr(vec![b("kind"), b("range"), b("est_rows"), b("3")]);
        assert_eq!(from_pair_list(&flat_explain).unwrap().columns.len(), 2);
        assert!(from_pair_list(&arr(vec![b("odd")])).is_none());
        assert!(from_query(&b("OK"), b"value").is_none());
        assert!(
            from_query(&arr(vec![Reply::Int(0), b("k"), b("v")]), b"value").is_none(),
            "cursor is text"
        );
        assert!(from_query(&arr(vec![]), b"value").is_none());
        let short_row = arr(vec![b("0"), arr(vec![arr(vec![b("user:1")])])]);
        assert!(from_query(&short_row, b"value").is_none(), "a row needs key and value");
        let bad_field =
            arr(vec![b("0"), arr(vec![arr(vec![b("k"), b("v"), Reply::Int(1), b("x")])])]);
        assert!(from_query(&bad_field, b"value").is_none(), "a field name must be text");
        let dangling = arr(vec![b("0"), arr(vec![arr(vec![b("k"), b("v"), b("name")])])]);
        assert_eq!(from_query(&dangling, b"value").unwrap().1.columns.len(), 2);
        assert!(from_advise(&b("OK")).is_none());
        assert!(from_advise(&arr(vec![arr(vec![b("x")])])).is_none());
        assert!(from_advise(&arr(vec![b("x")])).is_none());
    }

    #[test]
    fn explain_and_advise_shapes() {
        let explain = arr(vec![arr(vec![b("kind"), b("range")]), arr(vec![b("est_rows"), b("3")])]);
        let rows = from_pair_list(&explain).unwrap();
        assert_eq!((cols(&rows), rows.rows.len()), (vec!["kind".into(), "est_rows".into()], 1));
        let advise = arr(vec![arr(vec![Reply::Int(2), b("users.name"), b("TABLE.DECLARE …")])]);
        assert_eq!(from_advise(&advise).unwrap().rows[0][0], Cell::Int(2));
    }
}
