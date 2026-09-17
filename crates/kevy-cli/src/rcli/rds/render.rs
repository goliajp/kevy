//! Rows as text: an aligned table, TSV, CSV (RFC 4180), JSON, or one block
//! per record.

use super::rows::{Cell, Rows};

/// How rows are written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Format {
    /// Aligned columns and a row count; the terminal default.
    Table,
    /// Tab-separated, `\t` `\n` `\\` escaped; the default when piped.
    Tsv,
    Csv,
    /// An array of objects; a missing field is `null`.
    Json,
}

/// Everything that shapes the text besides the rows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Style {
    pub(crate) format: Format,
    pub(crate) header: bool,
    /// What a missing field shows as (not in JSON, which says `null`).
    pub(crate) null: Vec<u8>,
    /// One `column | value` block per row (table only).
    pub(crate) expanded: bool,
}

/// The rows in `style`.
pub(crate) fn render(rows: &Rows, style: &Style) -> Vec<u8> {
    match style.format {
        Format::Table if style.expanded => expanded(rows, style),
        Format::Table => table(rows, style),
        Format::Tsv => separated(rows, style, b'\t'),
        Format::Csv => separated(rows, style, b','),
        Format::Json => json(rows),
    }
}

fn shown(cell: &Cell, style: &Style) -> Vec<u8> {
    match cell {
        Cell::Text(t) => printable(t),
        Cell::Int(n) => n.to_string().into_bytes(),
        Cell::Null => style.null.clone(),
    }
}

/// Control bytes as `\xHH`, so a value cannot break the layout.
fn printable(text: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len());
    for &b in text {
        if b < 0x20 || b == 0x7f {
            out.extend_from_slice(format!("\\x{b:02x}").as_bytes());
        } else {
            out.push(b);
        }
    }
    out
}

/// Display width: characters of valid UTF-8, bytes otherwise.
fn width(text: &[u8]) -> usize {
    std::str::from_utf8(text).map_or(text.len(), |s| s.chars().count())
}

fn pad(out: &mut Vec<u8>, text: &[u8], to: usize, right: bool) {
    let fill = vec![b' '; to.saturating_sub(width(text))];
    if right {
        out.extend_from_slice(&fill);
        out.extend_from_slice(text);
    } else {
        out.extend_from_slice(text);
        out.extend_from_slice(&fill);
    }
}

fn table(rows: &Rows, style: &Style) -> Vec<u8> {
    let cells: Vec<Vec<Vec<u8>>> =
        rows.rows.iter().map(|r| r.iter().map(|c| shown(c, style)).collect()).collect();
    let widths: Vec<usize> = (0..rows.columns.len())
        .map(|i| {
            cells.iter().map(|r| width(&r[i])).chain([width(&rows.columns[i])]).max().unwrap_or(0)
        })
        .collect();
    let mut out = Vec::new();
    let line = |out: &mut Vec<u8>, parts: &[(Vec<u8>, bool)]| {
        for (i, (text, right)) in parts.iter().enumerate() {
            out.extend_from_slice(if i == 0 { b" " } else { b" | " });
            pad(out, text, widths[i], *right);
        }
        while out.last() == Some(&b' ') {
            out.pop();
        }
        out.push(b'\n');
    };
    if style.header {
        line(&mut out, &rows.columns.iter().map(|c| (c.clone(), false)).collect::<Vec<_>>());
        let rule: Vec<String> = widths.iter().map(|w| "-".repeat(w + 2)).collect();
        out.extend_from_slice(rule.join("+").as_bytes());
        out.push(b'\n');
    }
    for (r, row) in rows.rows.iter().zip(&cells) {
        let parts: Vec<(Vec<u8>, bool)> =
            r.iter().zip(row).map(|(c, t)| (t.clone(), matches!(c, Cell::Int(_)))).collect();
        line(&mut out, &parts);
    }
    let n = rows.rows.len();
    out.extend_from_slice(format!("({n} row{})\n", if n == 1 { "" } else { "s" }).as_bytes());
    out
}

fn expanded(rows: &Rows, style: &Style) -> Vec<u8> {
    let name_width = rows.columns.iter().map(|c| width(c)).max().unwrap_or(0);
    let mut out = Vec::new();
    for (i, row) in rows.rows.iter().enumerate() {
        let head = format!("-[ RECORD {} ]", i + 1);
        out.extend_from_slice(head.as_bytes());
        out.extend_from_slice("-".repeat((name_width + 1).saturating_sub(head.len())).as_bytes());
        out.extend_from_slice(b"+\n");
        for (name, cell) in rows.columns.iter().zip(row) {
            pad(&mut out, name, name_width, false);
            out.extend_from_slice(b" | ");
            out.extend_from_slice(&shown(cell, style));
            out.push(b'\n');
        }
    }
    out
}

fn separated(rows: &Rows, style: &Style, sep: u8) -> Vec<u8> {
    let field =
        |text: &[u8]| if sep == b',' { super::csv::field(text, b',') } else { tsv_field(text) };
    let mut out = Vec::new();
    let mut write = |fields: Vec<Vec<u8>>| {
        out.extend_from_slice(&fields.join(&sep));
        out.extend_from_slice(if sep == b',' { b"\r\n" } else { b"\n" });
    };
    if style.header {
        write(rows.columns.iter().map(|c| field(c)).collect());
    }
    for row in &rows.rows {
        write(
            row.iter()
                .map(|c| match c {
                    Cell::Text(t) => field(t),
                    Cell::Int(n) => n.to_string().into_bytes(),
                    Cell::Null => field(&style.null),
                })
                .collect(),
        );
    }
    out
}

fn tsv_field(text: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len());
    for &b in text {
        match b {
            b'\t' => out.extend_from_slice(b"\\t"),
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\r' => out.extend_from_slice(b"\\r"),
            b'\\' => out.extend_from_slice(b"\\\\"),
            _ => out.push(b),
        }
    }
    out
}

fn json(rows: &Rows) -> Vec<u8> {
    let objects: Vec<String> = rows
        .rows
        .iter()
        .map(|row| {
            let fields: Vec<String> = rows
                .columns
                .iter()
                .zip(row)
                .map(|(name, cell)| {
                    let value = match cell {
                        Cell::Text(t) => json_string(t),
                        Cell::Int(n) => n.to_string(),
                        Cell::Null => "null".to_string(),
                    };
                    format!("{}:{value}", json_string(name))
                })
                .collect();
            format!("{{{}}}", fields.join(","))
        })
        .collect();
    format!("[{}]\n", objects.join(",")).into_bytes()
}

/// A JSON string; bytes that are not UTF-8 become U+FFFD.
pub(crate) fn json_string(text: &[u8]) -> String {
    let mut out = String::from("\"");
    for ch in String::from_utf8_lossy(text).chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::{Format, Style, render};
    use crate::rcli::rds::rows::{Cell, Rows};

    fn sample() -> Rows {
        Rows {
            columns: vec![b"name".to_vec(), b"entries".to_vec(), b"note".to_vec()],
            rows: vec![
                vec![Cell::Text(b"users.age".to_vec()), Cell::Int(3), Cell::Null],
                vec![
                    Cell::Text(b"x".to_vec()),
                    Cell::Int(120),
                    Cell::Text(b"a,\"b\"\tc\n".to_vec()),
                ],
            ],
        }
    }

    fn style(format: Format) -> Style {
        Style { format, header: true, null: Vec::new(), expanded: false }
    }

    fn text(rows: &Rows, style: &Style) -> String {
        String::from_utf8(render(rows, style)).unwrap()
    }

    #[test]
    fn tables_align_and_count() {
        let want = " name      | entries | note\n-----------+---------+----------------\n users.age |       3 |\n x         |     120 | a,\"b\"\\x09c\\x0a\n(2 rows)\n";
        assert_eq!(text(&sample(), &style(Format::Table)), want);
        let quiet = Style { header: false, null: b"NULL".to_vec(), ..style(Format::Table) };
        assert!(text(&sample(), &quiet).starts_with(" users.age |       3 | NULL\n"));
        let one = Rows { rows: vec![sample().rows[0].clone()], ..sample() };
        assert!(text(&one, &style(Format::Table)).ends_with("(1 row)\n"));
    }

    #[test]
    fn separated_formats_quote_and_escape() {
        assert_eq!(
            text(&sample(), &style(Format::Tsv)),
            "name\tentries\tnote\nusers.age\t3\t\nx\t120\ta,\"b\"\\tc\\n\n"
        );
        assert_eq!(
            text(&sample(), &style(Format::Csv)),
            "name,entries,note\r\nusers.age,3,\r\nx,120,\"a,\"\"b\"\"\tc\n\"\r\n"
        );
        assert_eq!(
            text(&sample(), &style(Format::Json)),
            "[{\"name\":\"users.age\",\"entries\":3,\"note\":null},{\"name\":\"x\",\"entries\":120,\"note\":\"a,\\\"b\\\"\\tc\\n\"}]\n"
        );
    }

    #[test]
    fn expanded_blocks_per_record() {
        let s = Style { expanded: true, null: b"(null)".to_vec(), ..style(Format::Table) };
        let out = text(&sample(), &s);
        assert!(out.starts_with("-[ RECORD 1 ]+\nname    | users.age\nentries | 3\nnote    | (null)\n-[ RECORD 2 ]+\n"), "{out}");
    }
}
