//! `COMMAND` and its subcommands, answered from
//! [`crate::verb_meta::VERB_META`] (the single source of truth shared
//! with llms.txt and the MCP schema). An agent that can reach the
//! server can enumerate every verb, its arity, flags, and full syntax
//! without out-of-band knowledge.

use kevy_resp::{ArgvView, encode_array_len, encode_bulk, encode_error, encode_integer};

use crate::verb_meta::{VERB_META, VerbMeta, verb_meta};

/// Dispatch entry: `COMMAND [COUNT|LIST|INFO name…|DOCS [name…]]`.
pub(crate) fn cmd_command<A: ArgvView + ?Sized>(args: &A, out: &mut Vec<u8>) {
    match args.get(1) {
        None => {
            // Bare COMMAND: one Redis-shaped info row per verb.
            encode_array_len(out, VERB_META.len() as i64);
            for m in VERB_META {
                encode_info_row(out, m);
            }
        }
        Some(s) if s.eq_ignore_ascii_case(b"COUNT") => {
            encode_integer(out, VERB_META.len() as i64);
        }
        Some(s) if s.eq_ignore_ascii_case(b"LIST") => {
            encode_array_len(out, VERB_META.len() as i64);
            for m in VERB_META {
                encode_bulk(out, m.name.as_bytes());
            }
        }
        Some(s) if s.eq_ignore_ascii_case(b"INFO") => {
            encode_array_len(out, (args.len() - 2) as i64);
            for i in 2..args.len() {
                match args.get(i).and_then(lookup) {
                    Some(m) => encode_info_row(out, m),
                    None => out.extend_from_slice(b"*-1\r\n"),
                }
            }
        }
        Some(s) if s.eq_ignore_ascii_case(b"DOCS") => cmd_command_docs(args, out),
        Some(other) => {
            encode_error(
                out,
                &format!(
                    "ERR unknown COMMAND subcommand '{}' — try COUNT, LIST, INFO, or DOCS",
                    String::from_utf8_lossy(other)
                ),
            );
        }
    }
}

/// `COMMAND DOCS [name…]` body: no names = every verb.
fn cmd_command_docs<A: ArgvView + ?Sized>(args: &A, out: &mut Vec<u8>) {
    let named: Vec<&VerbMeta> = if args.len() > 2 {
        (2..args.len())
            .filter_map(|i| args.get(i).and_then(lookup))
            .collect()
    } else {
        VERB_META.iter().collect()
    };
    // Redis 7 DOCS shape: flat [name, fieldmap] pairs.
    encode_array_len(out, (named.len() * 2) as i64);
    for m in named {
        encode_bulk(out, m.name.as_bytes());
        encode_array_len(out, 10i64);
        for (k, v) in [
            ("summary", m.summary),
            ("since", m.since),
            ("group", m.group),
            ("syntax", m.syntax),
        ] {
            encode_bulk(out, k.as_bytes());
            encode_bulk(out, v.as_bytes());
        }
        encode_bulk(out, b"flags");
        encode_array_len(out, m.flags.len() as i64);
        for f in m.flags {
            encode_bulk(out, f.as_bytes());
        }
    }
}

fn lookup(name: &[u8]) -> Option<&'static VerbMeta> {
    let upper = String::from_utf8_lossy(name).to_ascii_uppercase();
    verb_meta(&upper)
}

/// Redis `COMMAND` 10-element info row. Key positions: first=1,
/// last=1, step=1 for keyed verbs is a simplification the DOCS
/// `syntax` string supersedes; extension verbs carry the "extension"
/// flag so clients know arg 1 is a catalog name, not a key.
fn encode_info_row(out: &mut Vec<u8>, m: &VerbMeta) {
    encode_array_len(out, 10i64);
    encode_bulk(out, m.name.to_ascii_lowercase().as_bytes());
    encode_integer(out, i64::from(m.arity));
    encode_array_len(out, m.flags.len() as i64);
    for f in m.flags {
        // Redis encodes flags as simple strings; bulk is accepted by
        // every client we test — keep the single encoder.
        encode_bulk(out, f.as_bytes());
    }
    encode_integer(out, 1); // first key
    encode_integer(out, 1); // last key
    encode_integer(out, 1); // step
    encode_array_len(out, 0i64); // acl categories
    encode_array_len(out, 0i64); // tips
    encode_array_len(out, 0i64); // key specs
    encode_array_len(out, 0i64); // subcommands
}
