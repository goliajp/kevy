//! What `help`, `help <command>` and `help @<group>` print.

use super::model::{Docs, Entry};

/// `help` alone.
pub(crate) fn overview() -> Vec<u8> {
    concat!(
        "kevy-cli ",
        env!("CARGO_PKG_VERSION"),
        "\nTo get help about commands type:
      \"help @<group>\" to get a list of commands in <group>
      \"help <command>\" for help on <command>
      \"help <tab>\" to get a list of possible help topics
      \"quit\" to exit

To set kevy-cli preferences:
      \":set hints\" enable online hints
      \":set nohints\" disable online hints
Set your preferences in ~/.kevyclirc
"
    )
    .as_bytes()
    .to_vec()
}

/// `help <topic words>`: every command in the group for `@group`, else every
/// entry the words are a prefix of (`help client` lists CLIENT's
/// subcommands). Nothing found prints just the closing line break.
pub(crate) fn topic(docs: &Docs, words: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    match words.first().and_then(|w| w.strip_prefix(b"@")) {
        Some(group) => {
            for e in &docs.entries {
                if e.group.as_deref().is_some_and(|g| g.eq_ignore_ascii_case(group)) {
                    block(&mut out, e, Detail::InGroup);
                }
            }
        }
        None => {
            for e in &docs.entries {
                let named = words.len() <= e.words.len()
                    && words.iter().zip(&e.words).all(|(a, b)| a.eq_ignore_ascii_case(b));
                if named {
                    block(&mut out, e, Detail::Alone);
                }
            }
        }
    }
    out.extend_from_slice(b"\r\n");
    out
}

/// A group listing leaves out what every entry in it shares.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Detail {
    InGroup,
    Alone,
}

fn block(out: &mut Vec<u8>, e: &Entry, detail: Detail) {
    let field = |out: &mut Vec<u8>, label: &[u8], value: &[u8]| {
        out.extend_from_slice(&[b"  \x1b[33m", label, b":\x1b[0m ", value, b"\r\n"].concat());
    };
    out.extend_from_slice(
        &[
            b"\r\n  \x1b[1m",
            e.full.as_slice(),
            b"\x1b[0m \x1b[90m",
            &e.params_text(),
            b"\x1b[0m\r\n",
        ]
        .concat(),
    );
    field(out, b"summary", e.summary.as_deref().unwrap_or_default());
    if let Some(since) = &e.since {
        field(out, b"since", since);
    }
    if detail == Detail::Alone {
        field(out, b"group", e.group.as_deref().unwrap_or_default());
        // kevy says how a command differs from Redis; "full" is no news.
        if let Some(compat) = e.compat.as_ref().filter(|c| c.as_slice() != b"full") {
            field(out, b"compat", compat);
        }
    }
}
