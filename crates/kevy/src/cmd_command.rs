//! `COMMAND` and its subcommands, answered from
//! [`crate::verb_meta::VERB_META`] (the single source of truth shared
//! with llms.txt and the MCP schema). An agent that can reach the
//! server can enumerate every verb, its arity, flags, and full syntax
//! without out-of-band knowledge.

use kevy_resp::{
    ArgvView, RespVersion, encode_array_len, encode_bulk, encode_error, encode_integer,
    encode_null_bulk,
};

use crate::cmd_command_info::{encode_row, getkeys};
use crate::verb_meta::{VERB_META, VerbMeta, verb_meta};

/// Dispatch entry: `COMMAND [COUNT | LIST | INFO [name ...] | DOCS [name ...]
/// | GETKEYS command [arg ...] | GETKEYSANDFLAGS command [arg ...]]`.
pub(crate) fn cmd_command<A: ArgvView + ?Sized>(args: &A, out: &mut Vec<u8>, proto: RespVersion) {
    let Some(sub) = args.get(1) else {
        // bare COMMAND: one Redis-shaped info row per verb
        encode_array_len(out, VERB_META.len() as i64);
        for m in VERB_META {
            encode_row(out, m, proto);
        }
        return;
    };
    match sub.to_ascii_uppercase().as_slice() {
        b"COUNT" if args.len() == 2 => encode_integer(out, VERB_META.len() as i64),
        b"COUNT" => encode_error(out, "ERR wrong number of arguments for 'command|count' command"),
        b"LIST" => {
            encode_array_len(out, VERB_META.len() as i64);
            for m in VERB_META {
                encode_bulk(out, m.name.as_bytes());
            }
        }
        b"INFO" => cmd_command_info(args, out, proto),
        b"DOCS" => cmd_command_docs(args, out),
        b"GETKEYS" => getkeys(args, false, proto, out),
        b"GETKEYSANDFLAGS" => getkeys(args, true, proto, out),
        _ => encode_error(
            out,
            &format!(
                "ERR unknown subcommand '{}'. Try COMMAND HELP.",
                String::from_utf8_lossy(sub)
            ),
        ),
    }
}

/// `COMMAND INFO [name ...]`: no names is every command; a subcommand is
/// named `container|sub`.
fn cmd_command_info<A: ArgvView + ?Sized>(args: &A, out: &mut Vec<u8>, proto: RespVersion) {
    if args.len() == 2 {
        encode_array_len(out, VERB_META.len() as i64);
        for m in VERB_META {
            encode_row(out, m, proto);
        }
        return;
    }
    encode_array_len(out, (args.len() - 2) as i64);
    for i in 2..args.len() {
        match lookup(&args[i]) {
            Some(m) => encode_row(out, m, proto),
            None => match crate::cmd_command_info::subcommand_row(&args[i]) {
                Some(row) => crate::cmd_command_info::encode_sub(out, row, proto),
                None if proto == RespVersion::V3 => out.extend_from_slice(b"_\r\n"),
                None => encode_null_bulk(out),
            },
        }
    }
}

/// `COMMAND DOCS [name…]` body: no names = every verb.
fn cmd_command_docs<A: ArgvView + ?Sized>(args: &A, out: &mut Vec<u8>) {
    let named: Vec<&VerbMeta> = if args.len() > 2 {
        (2..args.len()).filter_map(|i| args.get(i).and_then(lookup)).collect()
    } else {
        VERB_META.iter().collect()
    };
    // Redis 7 DOCS shape: flat [name, fieldmap] pairs.
    encode_array_len(out, (named.len() * 2) as i64);
    for m in named {
        encode_bulk(out, m.name.as_bytes());
        // 6 string fields + the flags array = 7 pairs = 14 slots.
        //
        // `complexity` and `compat` are not Redis DOCS fields, and that is the
        // point: an agent that can ask the server what a verb COSTS and how it
        // DIFFERS from Redis does not have to guess either. Both come from the
        // same registry the reference and llms.txt render, so they cannot drift.
        encode_array_len(out, 14i64);
        for (k, v) in [
            ("summary", m.summary),
            ("since", m.since),
            ("group", m.group),
            ("syntax", m.syntax),
            ("complexity", m.complexity),
            ("compat", m.compat),
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
