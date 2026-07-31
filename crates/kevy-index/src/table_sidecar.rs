//! The table-catalog sidecar line codec. Split from `table.rs` to keep
//! that file under the 500-LOC house rule.

use crate::catalog::{IndexKind, ValType};
use crate::table::{OrderPath, TableIndex, TableSpec, WindowSpec};

// `name<TAB>prefix<TAB>pk<TAB>columns<TAB>indexes<TAB>orderpaths`;
// columns = `n:ty,…`; indexes = `col:kind[:vcol]*,…` or `-`;
// orderpaths = `name:col:a|d[:col:a|d]*,…` or `-`. Names %-escape
// tab/newline/comma/colon/percent/non-print, so the separators can
// never split a name.

fn tesc(b: &[u8]) -> String {
    use core::fmt::Write as _;
    let mut out = String::with_capacity(b.len());
    for &c in b {
        if c == b'\t' || c == b'\n' || c == b'%' || c == b',' || c == b':' || !(32..127).contains(&c)
        {
            let _ = write!(out, "%{c:02X}");
        } else {
            out.push(c as char);
        }
    }
    if out.is_empty() { "%".into() } else { out }
}

fn tunesc(s: &str) -> Option<Vec<u8>> {
    if s == "%" {
        return Some(Vec::new());
    }
    let mut out = Vec::with_capacity(s.len());
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            out.push(u8::from_str_radix(s.get(i + 1..i + 3)?, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    Some(out)
}

pub(crate) fn spec_to_line(s: &TableSpec) -> String {
    let cols = s
        .columns
        .iter()
        .map(|(n, t)| format!("{}:{}", tesc(n), t.tag()))
        .collect::<Vec<_>>()
        .join(",");
    let idxs = if s.indexes.is_empty() {
        "-".into()
    } else {
        s.indexes
            .iter()
            .map(|ix| {
                let mut e = format!("{}:{}", tesc(&ix.column), ix.kind.tag());
                for v in &ix.values {
                    e.push(':');
                    e.push_str(&tesc(v));
                }
                e
            })
            .collect::<Vec<_>>()
            .join(",")
    };
    let ops = if s.orderpaths.is_empty() {
        "-".into()
    } else {
        s.orderpaths
            .iter()
            .map(|op| {
                let mut e = tesc(&op.name);
                for (col, desc) in &op.on {
                    e.push(':');
                    e.push_str(&tesc(col));
                    e.push(':');
                    e.push(if *desc { 'd' } else { 'a' });
                }
                e
            })
            .collect::<Vec<_>>()
            .join(",")
    };
    let mut line =
        format!("{}\t{}\t{}\t{}\t{}\t{}", tesc(&s.name), tesc(&s.prefix), tesc(&s.pk), cols, idxs, ops);
    // The window rides as an optional seventh field so a windowless
    // catalog stays byte-identical to the shape older readers know.
    if let Some(w) = &s.window {
        line.push_str(&format!("\t{}:{}:{}", tesc(&w.column), w.span, w.bucket));
    }
    line
}

pub(crate) fn spec_from_line(line: &str) -> Option<TableSpec> {
    let parts: Vec<&str> = line.split('\t').collect();
    if parts.len() != 6 && parts.len() != 7 {
        return None;
    }
    let window = match parts.get(6) {
        None => None,
        Some(f) => {
            let segs: Vec<&str> = f.split(':').collect();
            if segs.len() != 3 {
                return None;
            }
            Some(WindowSpec {
                column: tunesc(segs[0])?,
                span: segs[1].parse().ok()?,
                bucket: segs[2].parse().ok()?,
            })
        }
    };
    let columns = parts[3]
        .split(',')
        .map(|e| {
            let (n, t) = e.rsplit_once(':')?;
            Some((tunesc(n)?, ValType::parse(t.as_bytes())?))
        })
        .collect::<Option<Vec<_>>>()?;
    let indexes = if parts[4] == "-" {
        Vec::new()
    } else {
        parts[4].split(',').map(index_from_entry).collect::<Option<Vec<_>>>()?
    };
    let orderpaths = if parts[5] == "-" {
        Vec::new()
    } else {
        parts[5].split(',').map(orderpath_from_entry).collect::<Option<Vec<_>>>()?
    };
    Some(TableSpec {
        name: tunesc(parts[0])?,
        prefix: tunesc(parts[1])?,
        pk: tunesc(parts[2])?,
        columns,
        indexes,
        orderpaths,
        window,
    })
}

fn index_from_entry(e: &str) -> Option<TableIndex> {
    let mut segs = e.split(':');
    let column = tunesc(segs.next()?)?;
    let kind = IndexKind::parse(segs.next()?.as_bytes())?;
    let values = segs.map(tunesc).collect::<Option<Vec<_>>>()?;
    Some(TableIndex { column, kind, values })
}

fn orderpath_from_entry(e: &str) -> Option<OrderPath> {
    let segs: Vec<&str> = e.split(':').collect();
    if segs.len() < 3 || !(segs.len() - 1).is_multiple_of(2) {
        return None;
    }
    let name = tunesc(segs[0])?;
    let on = segs[1..]
        .chunks(2)
        .map(|pair| {
            let col = tunesc(pair[0])?;
            let desc = match pair[1] {
                "a" => false,
                "d" => true,
                _ => return None,
            };
            Some((col, desc))
        })
        .collect::<Option<Vec<_>>>()?;
    Some(OrderPath { name, on })
}

