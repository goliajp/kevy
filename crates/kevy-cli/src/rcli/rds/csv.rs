//! RFC 4180 records: fields split by a delimiter, double-quoted fields may
//! hold delimiters, quotes (doubled) and line breaks; CRLF or LF ends a
//! record.

/// One record and the byte offset just past it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Record {
    pub(crate) fields: Vec<Vec<u8>>,
    pub(crate) end: usize,
}

/// The record starting at `at`; `None` at the end of `text` or when a
/// quoted field is never closed.
pub(crate) fn next(text: &[u8], at: usize, delimiter: u8) -> Option<Record> {
    if at >= text.len() {
        return None;
    }
    let (mut fields, mut field, mut i) = (Vec::new(), Vec::new(), at);
    let mut quoted = false;
    while i < text.len() {
        let b = text[i];
        if quoted {
            match (b, text.get(i + 1)) {
                (b'"', Some(b'"')) => {
                    field.push(b'"');
                    i += 1;
                }
                (b'"', _) => quoted = false,
                _ => field.push(b),
            }
        } else if b == b'"' && field.is_empty() {
            quoted = true;
        } else if b == delimiter {
            fields.push(std::mem::take(&mut field));
        } else if b == b'\n' {
            fields.push(field);
            return Some(Record { fields, end: i + 1 });
        } else if !(b == b'\r' && text.get(i + 1) == Some(&b'\n')) {
            field.push(b);
        }
        i += 1;
    }
    if quoted {
        return None;
    }
    fields.push(field);
    Some(Record { fields, end: text.len() })
}

/// A field for writing: quoted when it holds the delimiter, a quote or a
/// line break.
pub(crate) fn field(text: &[u8], delimiter: u8) -> Vec<u8> {
    if !text.iter().any(|&b| b == delimiter || matches!(b, b'"' | b'\r' | b'\n')) {
        return text.to_vec();
    }
    let mut out = vec![b'"'];
    for &b in text {
        if b == b'"' {
            out.push(b'"');
        }
        out.push(b);
    }
    out.push(b'"');
    out
}

#[cfg(test)]
mod tests {
    use super::{field, next};

    fn fields(text: &[u8], at: usize) -> (Vec<String>, usize) {
        let r = next(text, at, b',').unwrap();
        (r.fields.iter().map(|f| String::from_utf8_lossy(f).into_owned()).collect(), r.end)
    }

    #[test]
    fn records_split_on_delimiters_outside_quotes() {
        let text = b"id,name,note\r\n1,\"a, b\",\"say \"\"hi\"\"\"\n2,,\"two\nlines\"\n3,x";
        assert_eq!(fields(text, 0), (vec!["id".into(), "name".into(), "note".into()], 14));
        let (second, end) = fields(text, 14);
        assert_eq!(second, ["1", "a, b", "say \"hi\""]);
        let (third, end) = fields(text, end);
        assert_eq!(third, ["2", "", "two\nlines"]);
        assert_eq!(fields(text, end).0, ["3", "x"]);
        assert!(next(b"1,\"open", 0, b',').is_none());
        assert!(next(b"", 0, b',').is_none());
        assert_eq!(next(b"a\tb\n", 0, b'\t').unwrap().fields.len(), 2);
    }

    #[test]
    fn fields_are_quoted_only_when_needed() {
        assert_eq!(field(b"plain", b','), b"plain");
        assert_eq!(field(b"a,b", b','), b"\"a,b\"");
        assert_eq!(field(b"say \"hi\"", b','), b"\"say \"\"hi\"\"\"");
        assert_eq!(field(b"a,b", b'\t'), b"a,b");
    }
}
