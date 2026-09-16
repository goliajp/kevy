//! The command reference help, hints and completion read, built once from a
//! `COMMAND DOCS` reply.
//!
//! Two servers describe arguments two ways: Redis and Valkey send a typed
//! `arguments` tree, which hints can match typed words against; kevy sends a
//! `syntax` line, which can only be shown whole. Both land in [`Params`], so
//! nothing downstream has to know which server answered.

use kevy_resp::Reply;

/// What a word must look like to be taken as an argument's value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Value {
    /// Any word (`key`, `string`, `pattern`, and types this client does not know).
    Any,
    /// Leading blanks, a sign, then at least one digit (`integer`, `unix-time`).
    Integer,
    /// A leading floating-point number (`double`).
    Double,
}

/// An argument's structure.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Shape {
    Value(Value),
    /// The token alone (`NX`).
    Token,
    /// Exactly one of the children.
    OneOf(Vec<Arg>),
    /// Every child, in order.
    Block(Vec<Arg>),
}

/// Whether an argument may repeat, and whether its token repeats with it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Repeat {
    Once,
    Many,
    /// `WEIGHTS w [w ...]` repeats the value; this repeats `TOKEN value` pairs.
    ManyWithToken,
}

/// One node of a command's argument tree.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Arg {
    /// `display_text`, else `name`.
    pub(crate) display: Vec<u8>,
    pub(crate) token: Option<Vec<u8>>,
    pub(crate) shape: Shape,
    pub(crate) optional: bool,
    pub(crate) repeat: Repeat,
}

/// How a command's arguments are described.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Params {
    /// A typed tree (Redis, Valkey).
    Args(Vec<Arg>),
    /// A syntax line without the command's own words (kevy).
    Syntax(Vec<u8>),
    /// Nothing: the command takes no arguments, or the server did not say.
    Unknown,
}

/// One command, or one subcommand (`CLIENT KILL`).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Entry {
    /// Upper-cased: `[CLIENT, KILL]`.
    pub(crate) words: Vec<Vec<u8>>,
    /// The words joined by a space.
    pub(crate) full: Vec<u8>,
    pub(crate) summary: Option<Vec<u8>>,
    pub(crate) since: Option<Vec<u8>>,
    pub(crate) group: Option<Vec<u8>>,
    /// kevy's statement of how the command differs from Redis.
    pub(crate) compat: Option<Vec<u8>>,
    pub(crate) params: Params,
}

/// Every entry, sorted by `full` byte order, and every group named.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Docs {
    pub(crate) entries: Vec<Entry>,
    /// Sorted, without duplicates.
    pub(crate) groups: Vec<Vec<u8>>,
}

/// kevy's own `COMMAND DOCS` reply, for when there is no server to ask. The
/// kevy crate's tests hold it equal to what the server answers.
const OFFLINE: &[u8] = include_bytes!("kevy-command-docs.resp");

impl Docs {
    /// Build from a `COMMAND DOCS` reply; `None` when it is not a docs table.
    pub(crate) fn from_reply(reply: &Reply) -> Option<Docs> {
        let mut docs = Docs::default();
        for (name, spec) in pairs(reply)? {
            add_entry(&mut docs, &[text(name)?], spec)?;
        }
        docs.entries.sort_by(|a, b| a.full.cmp(&b.full));
        docs.groups.sort();
        docs.groups.dedup();
        Some(docs)
    }

    /// kevy's own reference.
    pub(crate) fn offline() -> Docs {
        kevy_resp::parse_reply(OFFLINE)
            .ok()
            .flatten()
            .and_then(|(reply, _)| Docs::from_reply(&reply))
            .unwrap_or_default()
    }
}

/// One entry from its spec map, then its subcommands.
fn add_entry(docs: &mut Docs, names: &[&[u8]], spec: &Reply) -> Option<()> {
    let words: Vec<Vec<u8>> = names.iter().map(|n| n.to_ascii_uppercase()).collect();
    let mut entry = Entry {
        full: words.join(&b' '),
        words,
        summary: None,
        since: None,
        group: None,
        compat: None,
        params: Params::Unknown,
    };
    let mut syntax = None;
    for (key, value) in pairs(spec)? {
        match text(key)? {
            b"summary" => entry.summary = text(value).map(<[u8]>::to_vec),
            b"since" => entry.since = text(value).map(<[u8]>::to_vec),
            b"group" => entry.group = text(value).map(<[u8]>::to_vec),
            b"compat" => entry.compat = text(value).map(<[u8]>::to_vec),
            b"syntax" => syntax = text(value),
            b"arguments" => entry.params = Params::Args(args(value)?),
            b"subcommands" => add_subcommands(docs, names[0], value)?,
            _ => {}
        }
    }
    if let (Params::Unknown, Some(line)) = (&entry.params, syntax) {
        entry.params = Params::Syntax(strip_words(line, &entry.words));
    }
    if let Some(group) = &entry.group {
        docs.groups.push(group.clone());
    }
    docs.entries.push(entry);
    Some(())
}

/// Subcommands are named `container|sub`; the entry is `CONTAINER SUB`.
fn add_subcommands(docs: &mut Docs, container: &[u8], table: &Reply) -> Option<()> {
    for (name, spec) in pairs(table)? {
        let name = text(name)?;
        let sub = match name.iter().position(|&b| b == b'|') {
            Some(bar) => &name[bar + 1..],
            None => name,
        };
        add_entry(docs, &[container, sub], spec)?;
    }
    Some(())
}

fn args(list: &Reply) -> Option<Vec<Arg>> {
    let Reply::Array(items) = list else { return None };
    items.iter().map(arg).collect()
}

fn arg(spec: &Reply) -> Option<Arg> {
    let (mut name, mut display, mut token, mut kind, mut children) = (None, None, None, None, None);
    let (mut optional, mut multiple, mut multiple_token) = (false, false, false);
    for (key, value) in pairs(spec)? {
        match text(key)? {
            b"name" => name = text(value),
            b"display_text" => display = text(value),
            b"token" => token = text(value).map(<[u8]>::to_vec),
            b"type" => kind = text(value),
            b"arguments" => children = Some(args(value)?),
            b"flags" => {
                for flag in items(value)? {
                    match text(flag)? {
                        b"optional" => optional = true,
                        b"multiple" => multiple = true,
                        b"multiple_token" => multiple_token = true,
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    let children = children.unwrap_or_default();
    let shape = match kind {
        Some(b"integer" | b"unix-time") => Shape::Value(Value::Integer),
        Some(b"double") => Shape::Value(Value::Double),
        Some(b"pure-token") => Shape::Token,
        Some(b"oneof") => Shape::OneOf(children),
        Some(b"block") => Shape::Block(children),
        _ => Shape::Value(Value::Any),
    };
    let repeat = match (multiple, multiple_token) {
        (false, _) => Repeat::Once,
        (true, false) => Repeat::Many,
        (true, true) => Repeat::ManyWithToken,
    };
    let display = display.or(name).unwrap_or_default().to_vec();
    Some(Arg { display, token, shape, optional, repeat })
}

/// A syntax line without the leading words that name the command.
fn strip_words(line: &[u8], words: &[Vec<u8>]) -> Vec<u8> {
    let mut rest = line;
    for word in words {
        let trimmed = rest.trim_ascii_start();
        let end = trimmed.iter().position(u8::is_ascii_whitespace).unwrap_or(trimmed.len());
        if !trimmed[..end].eq_ignore_ascii_case(word) {
            break;
        }
        rest = &trimmed[end..];
    }
    rest.trim_ascii().to_vec()
}

/// A map, or an array of alternating keys and values.
fn pairs(reply: &Reply) -> Option<Vec<(&Reply, &Reply)>> {
    match reply {
        Reply::Map(entries) => Some(entries.iter().map(|(k, v)| (k, v)).collect()),
        Reply::Array(flat) if flat.len() % 2 == 0 => {
            Some(flat.as_chunks::<2>().0.iter().map(|[k, v]| (k, v)).collect())
        }
        _ => None,
    }
}

fn items(reply: &Reply) -> Option<&[Reply]> {
    match reply {
        Reply::Array(v) | Reply::Set(v) => Some(v),
        _ => None,
    }
}

fn text(reply: &Reply) -> Option<&[u8]> {
    match reply {
        Reply::Bulk(b) | Reply::Simple(b) => Some(b),
        Reply::Verbatim { data, .. } => Some(data),
        _ => None,
    }
}
