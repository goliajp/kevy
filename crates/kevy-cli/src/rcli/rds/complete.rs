//! Completing the object name after a verb that takes one: tables after
//! TABLE.VERIFY or `\d`, indexes after IDX.QUERY, views after VIEW.QUERY.

use super::rows::{Cell, from_pair_rows};
use crate::rcli::session::Session;

/// The names the server declares, read once per prompt.
#[derive(Clone, Debug, Default)]
pub(crate) struct Catalog {
    tables: Vec<Vec<u8>>,
    indexes: Vec<Vec<u8>>,
    views: Vec<Vec<u8>>,
}

/// Read the three catalogs; a server without them leaves them empty.
pub(crate) fn load(s: &mut Session) -> Catalog {
    let mut names = |verb: &[u8]| -> Vec<Vec<u8>> {
        let Ok(reply) = s.request(&[verb]) else { return Vec::new() };
        let Some(rows) = from_pair_rows(&reply) else { return Vec::new() };
        let Some(at) = rows.columns.iter().position(|c| c == b"name") else { return Vec::new() };
        rows.rows
            .iter()
            .filter_map(|r| if let Cell::Text(n) = &r[at] { Some(n.clone()) } else { None })
            .collect()
    };
    Catalog {
        tables: names(b"TABLE.LIST"),
        indexes: names(b"IDX.LIST"),
        views: names(b"VIEW.LIST"),
    }
}

/// Whole-line candidates for `<verb> <partial name>`; none for other lines.
pub(crate) fn completions(catalog: &Catalog, line: &[u8]) -> Vec<Vec<u8>> {
    let Some(space) = line.iter().position(|&b| b == b' ') else { return Vec::new() };
    let (verb, partial) = (&line[..space], &line[space + 1..]);
    if partial.contains(&b' ') {
        return Vec::new();
    }
    let upper = verb.to_ascii_uppercase();
    let list = match upper.as_slice() {
        b"TABLE.VERIFY" | b"TABLE.DROP" | b"\\D" | b"\\D+" | b"DESCRIBE" | b"DESCRIBE+" => {
            &catalog.tables
        }
        b"IDX.QUERY" | b"IDX.EXPLAIN" | b"IDX.VERIFY" | b"IDX.DROP" | b"IDX.COUNT" | b"EXPLAIN" => {
            &catalog.indexes
        }
        b"VIEW.QUERY" | b"VIEW.EXPLAIN" | b"VIEW.VERIFY" | b"VIEW.DROP" => &catalog.views,
        _ => return Vec::new(),
    };
    let mut out: Vec<Vec<u8>> = list
        .iter()
        .filter(|n| n.starts_with(partial))
        .map(|n| [&line[..=space], n.as_slice()].concat())
        .collect();
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::{Catalog, completions};

    #[test]
    fn names_follow_the_verbs_that_take_them() {
        let catalog = Catalog {
            tables: vec![b"users".to_vec(), b"orders".to_vec()],
            indexes: vec![b"users.age".to_vec(), b"users.by_age".to_vec()],
            views: vec![b"young".to_vec()],
        };
        let words = |line: &str| {
            completions(&catalog, line.as_bytes())
                .into_iter()
                .map(|c| String::from_utf8(c).unwrap())
                .collect::<Vec<_>>()
        };
        assert_eq!(words("idx.query users."), ["idx.query users.age", "idx.query users.by_age"]);
        assert_eq!(words("\\d u"), ["\\d users"]);
        assert_eq!(words("VIEW.QUERY "), ["VIEW.QUERY young"]);
        assert!(words("IDX.QUERY users.age RANGE").is_empty());
        assert!(words("GET us").is_empty());
        assert!(words("TABLE.VERIFY").is_empty());
    }
}
