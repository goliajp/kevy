//! `TABLE.DESCRIBE` / `IDX.DESCRIBE` / `VIEW.DESCRIBE` replies: fetched,
//! read by label, and their `declaration` written as a command line that
//! `run -f` splits back into the same argv.

use crate::rcli::session::{Session, eprint_bytes};
use kevy_resp::Reply;

/// Which catalog a name was found in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Table,
    Index,
    View,
}

impl Kind {
    fn verb(self) -> &'static [u8] {
        match self {
            Kind::Table => b"TABLE.DESCRIBE",
            Kind::Index => b"IDX.DESCRIBE",
            Kind::View => b"VIEW.DESCRIBE",
        }
    }

    pub(crate) fn noun(self) -> &'static str {
        match self {
            Kind::Table => "table",
            Kind::Index => "index",
            Kind::View => "view",
        }
    }
}

/// One describe reply, label/value.
pub(crate) struct Described {
    pub(crate) kind: Kind,
    fields: Vec<Reply>,
}

/// Describe `name` in `kind`'s catalog: `Ok(None)` when it holds no such
/// name, `Err` after printing any other failure (a server without the
/// verb, a lost link).
pub(crate) fn fetch(s: &mut Session, kind: Kind, name: &[u8]) -> Result<Option<Described>, ()> {
    match s.request(&[kind.verb(), name]) {
        Ok(reply @ Reply::Array(_)) => Ok(from_reply(kind, reply)),
        Ok(Reply::Error(msg)) if msg.starts_with(b"ERR no such ") => Ok(None),
        Ok(Reply::Error(msg)) => {
            eprint_bytes(&[b"(error) ", &msg, b"\n"]);
            Err(())
        }
        Ok(_) => {
            eprint_bytes(&[b"kevy-cli: ", kind.verb(), b" answered in an unknown shape\n"]);
            Err(())
        }
        Err(e) => {
            eprint_bytes(&[b"Error: ", e.text().as_bytes(), b"\n"]);
            Err(())
        }
    }
}

/// A describe reply already in hand; `None` for any other reply.
pub(crate) fn from_reply(kind: Kind, reply: Reply) -> Option<Described> {
    match reply {
        Reply::Array(fields) => Some(Described { kind, fields }),
        _ => None,
    }
}

/// `name` as a table, else an index, else a view.
pub(crate) fn find(s: &mut Session, name: &[u8]) -> Result<Option<Described>, ()> {
    for kind in [Kind::Table, Kind::Index, Kind::View] {
        if let Some(d) = fetch(s, kind, name)? {
            return Ok(Some(d));
        }
    }
    Ok(None)
}

impl Described {
    /// The value after `label`.
    pub(crate) fn field(&self, label: &[u8]) -> Option<&Reply> {
        let at = self.fields.chunks(2).position(|kv| text(&kv[0]) == Some(label))?;
        self.fields.get(at * 2 + 1)
    }

    pub(crate) fn text(&self, label: &[u8]) -> Option<&[u8]> {
        self.field(label).and_then(text)
    }

    /// The argv that recreates the object; `None` where it has none of its
    /// own (an index a table compiled).
    pub(crate) fn declaration(&self) -> Option<Vec<Vec<u8>>> {
        words(self.field(b"declaration")?)
    }

    /// A table's `(column, type)` pairs, declaration order.
    pub(crate) fn columns(&self) -> Vec<(Vec<u8>, Vec<u8>)> {
        let Some(Reply::Array(cols)) = self.field(b"columns") else { return Vec::new() };
        cols.iter()
            .filter_map(|c| match words(c)?.as_slice() {
                [name, ty] => Some((name.clone(), ty.clone())),
                _ => None,
            })
            .collect()
    }
}

fn text(reply: &Reply) -> Option<&[u8]> {
    match reply {
        Reply::Bulk(b) | Reply::Simple(b) => Some(b),
        _ => None,
    }
}

/// An array of bulk strings as words.
pub(crate) fn words(reply: &Reply) -> Option<Vec<Vec<u8>>> {
    let Reply::Array(items) = reply else { return None };
    items.iter().map(|i| text(i).map(<[u8]>::to_vec)).collect()
}

/// `argv` as one line: plain words as they are, anything the splitter
/// would read differently (space, quote, backslash, a control or non-ASCII
/// byte, the empty word) in redis-cli's double-quoted form.
pub(crate) fn command_line(argv: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    for (i, word) in argv.iter().enumerate() {
        if i > 0 {
            out.push(b' ');
        }
        let plain = !word.is_empty()
            && word.iter().all(|&b| b.is_ascii_graphic() && !matches!(b, b'"' | b'\'' | b'\\'));
        if plain {
            out.extend_from_slice(word);
        } else {
            crate::rcli::repr::push_repr(&mut out, word);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::command_line;
    use crate::rcli::splitargs::split_args;

    #[test]
    fn a_command_line_splits_back_into_the_same_words() {
        let argv: Vec<Vec<u8>> = vec![
            b"VIEW.CREATE".to_vec(),
            b"v".to_vec(),
            b"(".to_vec(),
            b"two words".to_vec(),
            b"it's".to_vec(),
            b"a\"b\\c".to_vec(),
            b"".to_vec(),
            b"\x00\xff\n".to_vec(),
            b"user:{key}".to_vec(),
        ];
        let line = command_line(&argv);
        assert_eq!(
            line,
            br#"VIEW.CREATE v ( "two words" "it's" "a\"b\\c" "" "\x00\xff\n" user:{key}"#.to_vec()
        );
        assert_eq!(split_args(&line), Some(argv));
    }
}
