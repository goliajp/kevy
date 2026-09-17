//! Completing the object name after a verb that takes one: tables after
//! TABLE.VERIFY or `\d`, indexes after IDX.QUERY, views after VIEW.QUERY —
//! and a table's columns where an `IDX.QUERY <table>.<path>` names one
//! (after FIELDS, FILTER, SORT, DISTINCT, FACET).

use crate::rcli::session::Session;

/// The names the server declares, read once per prompt.
#[derive(Clone, Debug, Default)]
pub(crate) struct Catalog {
    tables: Vec<Vec<u8>>,
    indexes: Vec<Vec<u8>>,
    views: Vec<Vec<u8>>,
    /// Each table's declared columns.
    columns: Vec<(Vec<u8>, Vec<Vec<u8>>)>,
}

/// Read the three catalogs; a server without them leaves them empty.
pub(crate) fn load(s: &mut Session) -> Catalog {
    let mut names = |verb: &[u8]| super::catalog::names(s, verb).unwrap_or_default();
    let (tables, indexes, views) = (names(b"TABLE.LIST"), names(b"IDX.LIST"), names(b"VIEW.LIST"));
    let columns = columns_of(s, &tables);
    Catalog { tables, indexes, views, columns }
}

/// Every table's columns, one pipelined round trip; a server without
/// TABLE.DESCRIBE leaves them empty.
fn columns_of(s: &mut Session, tables: &[Vec<u8>]) -> Vec<(Vec<u8>, Vec<Vec<u8>>)> {
    let commands: Vec<Vec<&[u8]>> =
        tables.iter().map(|t| vec![&b"TABLE.DESCRIBE"[..], t.as_slice()]).collect();
    if commands.is_empty() {
        return Vec::new();
    }
    let Ok(replies) = s.pipeline(&commands) else { return Vec::new() };
    tables
        .iter()
        .zip(replies)
        .filter_map(|(t, reply)| {
            let d = super::described::from_reply(super::described::Kind::Table, reply)?;
            Some((t.clone(), d.columns().into_iter().map(|(c, _)| c).collect()))
        })
        .collect()
}

/// Whole-line candidates for `<verb> <partial name>`; none for other lines.
pub(crate) fn completions(catalog: &Catalog, line: &[u8]) -> Vec<Vec<u8>> {
    if let Some(columns) = column_completions(catalog, line) {
        return columns;
    }
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

/// Columns of the table an `IDX.QUERY <table>.<path>` line names, where the
/// word being typed is one; `None` when it is not a column position.
fn column_completions(catalog: &Catalog, line: &[u8]) -> Option<Vec<Vec<u8>>> {
    let words: Vec<&[u8]> = line.split(|&b| b == b' ').collect();
    let (partial, before) = words.split_last()?;
    let [verb, path, rest @ ..] = before else { return None };
    if !verb.eq_ignore_ascii_case(b"IDX.QUERY") && !verb.eq_ignore_ascii_case(b"IDX.COUNT") {
        return None;
    }
    let is = |w: &[u8], kw: &[u8]| w.eq_ignore_ascii_case(kw);
    let after_keyword = rest.last().is_some_and(|w| {
        [&b"FILTER"[..], b"SORT", b"DISTINCT", b"FACET", b"FIELDS"].iter().any(|kw| is(w, kw))
    });
    let in_fields = rest.iter().any(|w| is(w, b"FIELDS"));
    if !after_keyword && !in_fields {
        return None;
    }
    let table = &path[..path.iter().position(|&b| b == b'.')?];
    let (_, columns) = catalog.columns.iter().find(|(t, _)| t == table)?;
    let stem = &line[..line.len() - partial.len()];
    let mut out: Vec<Vec<u8>> = columns
        .iter()
        .filter(|c| c.starts_with(partial))
        .map(|c| [stem, c.as_slice()].concat())
        .collect();
    out.sort();
    Some(out)
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
            columns: vec![(b"users".to_vec(), vec![b"age".to_vec(), b"name".to_vec()])],
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
        assert_eq!(
            words("IDX.QUERY users.age RANGE 1 9 FIELDS name a"),
            ["IDX.QUERY users.age RANGE 1 9 FIELDS name age"]
        );
        assert_eq!(
            words("idx.query users.age RANGE 1 9 SORT "),
            ["idx.query users.age RANGE 1 9 SORT age", "idx.query users.age RANGE 1 9 SORT name"]
        );
        assert!(words("IDX.QUERY users.age RANGE 1 ").is_empty(), "a bound is not a column");
        assert!(words("IDX.QUERY orders.at RANGE 1 9 FIELDS ").is_empty(), "no columns known");
    }
}
