//! The `COMMAND INFO` row and the `GETKEYS` / `GETKEYSANDFLAGS` replies,
//! in the shape Redis gives each protocol: plain arrays under RESP2, sets
//! and maps under RESP3.

use kevy_resp::{
    ArgvView, RespVersion, encode_array_len, encode_bulk, encode_error, encode_integer,
    encode_map_header, encode_set_header, encode_simple_string,
};

use crate::command_spec::{Begin, CmdSpec, Find, KeySpec, c, k, keys_of, spec};
use crate::verb_meta::{VerbMeta, verb_meta};

/// kevy's own commands that take a key; the rest of them take none.
const KEVY_KEYED: &[CmdSpec] = &[c(
    "zpopmin.below",
    -3,
    &[],
    1,
    1,
    1,
    &[],
    &[],
    &[k("", &["RW", "access", "delete"], Begin::Index(1), Find::Range(0, 1, 0))],
    &[],
)];

fn kevy_keyed(name: &str) -> Option<&'static CmdSpec> {
    KEVY_KEYED.iter().find(|s| s.name.eq_ignore_ascii_case(name))
}

fn set_header(out: &mut Vec<u8>, n: usize, proto: RespVersion) {
    match proto {
        RespVersion::V3 => encode_set_header(out, n as i64),
        RespVersion::V2 => encode_array_len(out, n as i64),
    }
}

fn map_header(out: &mut Vec<u8>, n: usize, proto: RespVersion) {
    match proto {
        RespVersion::V3 => encode_map_header(out, n as i64),
        RespVersion::V2 => encode_array_len(out, 2 * n as i64),
    }
}

fn simple_strings(out: &mut Vec<u8>, items: &[&str], proto: RespVersion) {
    set_header(out, items.len(), proto);
    for i in items {
        encode_simple_string(out, i);
    }
}

/// A command's row: Redis's own for a command Redis also has, kevy's
/// flags with no ACL categories or tips for one of kevy's own.
pub(crate) fn encode_row(out: &mut Vec<u8>, m: &VerbMeta, proto: RespVersion) {
    if let Some(s) = spec(m.name.as_bytes()) {
        // the arity this server takes, where its syntax departs from Redis's
        return encode_spec_row(out, s, i32::from(m.arity), proto);
    }
    let keyed = kevy_keyed(m.name);
    encode_array_len(out, 10);
    encode_bulk(out, m.name.to_ascii_lowercase().as_bytes());
    encode_integer(out, i64::from(m.arity));
    simple_strings(out, m.flags, proto);
    let (first, last, step) = keyed.map_or((0, 0, 0), |s| (s.first, s.last, s.step));
    for n in [first, last, step] {
        encode_integer(out, i64::from(n));
    }
    set_header(out, 0, proto);
    set_header(out, 0, proto);
    let specs = keyed.map_or(&[][..], |s| s.keys);
    set_header(out, specs.len(), proto);
    for ks in specs {
        encode_key_spec(out, ks, proto);
    }
    set_header(out, 0, proto);
}

fn encode_spec_row(out: &mut Vec<u8>, s: &CmdSpec, arity: i32, proto: RespVersion) {
    encode_array_len(out, 10);
    encode_bulk(out, s.name.as_bytes());
    encode_integer(out, i64::from(arity));
    simple_strings(out, s.flags, proto);
    for n in [s.first, s.last, s.step] {
        encode_integer(out, i64::from(n));
    }
    simple_strings(out, s.acl, proto);
    set_header(out, s.tips.len(), proto);
    for t in s.tips {
        encode_bulk(out, t.as_bytes());
    }
    set_header(out, s.keys.len(), proto);
    for ks in s.keys {
        encode_key_spec(out, ks, proto);
    }
    // none is an empty set like the lists above; some are an array, under
    // either protocol
    if s.subs.is_empty() {
        set_header(out, 0, proto);
    } else {
        encode_array_len(out, s.subs.len() as i64);
    }
    for sub in s.subs {
        encode_spec_row(out, sub, sub.arity, proto);
    }
}

fn pair_int(out: &mut Vec<u8>, name: &str, v: i32) {
    encode_bulk(out, name.as_bytes());
    encode_integer(out, i64::from(v));
}

fn encode_key_spec(out: &mut Vec<u8>, ks: &KeySpec, proto: RespVersion) {
    map_header(out, if ks.notes.is_empty() { 3 } else { 4 }, proto);
    if !ks.notes.is_empty() {
        encode_bulk(out, b"notes");
        encode_bulk(out, ks.notes.as_bytes());
    }
    encode_bulk(out, b"flags");
    simple_strings(out, ks.flags, proto);
    encode_bulk(out, b"begin_search");
    map_header(out, 2, proto);
    encode_bulk(out, b"type");
    encode_begin(out, &ks.begin, proto);
    encode_bulk(out, b"find_keys");
    map_header(out, 2, proto);
    encode_bulk(out, b"type");
    encode_find(out, &ks.find, proto);
}

fn encode_begin(out: &mut Vec<u8>, begin: &Begin, proto: RespVersion) {
    match *begin {
        Begin::Index(pos) => {
            encode_bulk(out, b"index");
            encode_bulk(out, b"spec");
            map_header(out, 1, proto);
            pair_int(out, "index", pos);
        }
        Begin::Keyword(word, from) => {
            encode_bulk(out, b"keyword");
            encode_bulk(out, b"spec");
            map_header(out, 2, proto);
            encode_bulk(out, b"keyword");
            encode_bulk(out, word.as_bytes());
            pair_int(out, "startfrom", from);
        }
        Begin::Unknown => unknown_spec(out, proto),
    }
}

fn encode_find(out: &mut Vec<u8>, find: &Find, proto: RespVersion) {
    let (kind, fields) = match *find {
        Find::Range(last, step, limit) => {
            ("range", [("lastkey", last), ("keystep", step), ("limit", limit)])
        }
        Find::KeyNum(idx, first, step) => {
            ("keynum", [("keynumidx", idx), ("firstkey", first), ("keystep", step)])
        }
        Find::Unknown => return unknown_spec(out, proto),
    };
    encode_bulk(out, kind.as_bytes());
    encode_bulk(out, b"spec");
    map_header(out, 3, proto);
    for (name, v) in fields {
        pair_int(out, name, v);
    }
}

fn unknown_spec(out: &mut Vec<u8>, proto: RespVersion) {
    encode_bulk(out, b"unknown");
    encode_bulk(out, b"spec");
    map_header(out, 0, proto);
}

/// The row `argv` names: a command, or a container command's subcommand.
/// `None` is a command neither Redis nor kevy has.
fn resolve(argv: &[&[u8]]) -> Option<&'static CmdSpec> {
    let name = std::str::from_utf8(argv[0]).ok()?;
    let top = spec(argv[0]).or_else(|| kevy_keyed(name));
    match top {
        Some(s) if !s.subs.is_empty() && argv.len() >= 2 => {
            let full = format!("{}|{}", s.name, String::from_utf8_lossy(argv[1]));
            s.subs.iter().find(|sub| sub.name.eq_ignore_ascii_case(&full))
        }
        Some(s) => Some(s),
        None => verb_meta(&name.to_ascii_uppercase()).map(|_| &KEYLESS),
    }
}

/// Stands in for one of kevy's own commands that take no key.
static KEYLESS: CmdSpec = c("", -1, &[], 0, 0, 0, &[], &[], &[], &[]);

/// `COMMAND GETKEYS | GETKEYSANDFLAGS command [arg ...]`.
pub(crate) fn getkeys<A: ArgvView + ?Sized>(
    args: &A,
    with_flags: bool,
    proto: RespVersion,
    out: &mut Vec<u8>,
) {
    if args.len() < 3 {
        let sub = if with_flags { "getkeysandflags" } else { "getkeys" };
        return encode_error(
            out,
            &format!("ERR wrong number of arguments for 'command|{sub}' command"),
        );
    }
    let argv: Vec<&[u8]> = (2..args.len()).map(|i| &args[i]).collect();
    let Some(cmd) = resolve(&argv) else {
        return encode_error(out, "ERR Invalid command specified");
    };
    if !cmd.keys.iter().any(|s| !s.flags.contains(&"not_key")) {
        return encode_error(out, "ERR The command has no key arguments");
    }
    let argc = argv.len() as i32;
    if (cmd.arity > 0 && cmd.arity != argc) || argc < -cmd.arity {
        return encode_error(out, "ERR Invalid number of arguments specified for command");
    }
    let keys = keys_of(cmd, &argv);
    if keys.is_empty() {
        if cmd.flags.contains(&"no_mandatory_keys") {
            return encode_array_len(out, 0);
        }
        return encode_error(out, "ERR Invalid arguments specified for command");
    }
    encode_array_len(out, keys.len() as i64);
    for (at, flags) in keys {
        if with_flags {
            encode_array_len(out, 2);
            encode_bulk(out, argv[at]);
            simple_strings(out, flags, proto);
        } else {
            encode_bulk(out, argv[at]);
        }
    }
}

/// A subcommand's row, by its `container|sub` name.
pub(crate) fn subcommand_row(name: &[u8]) -> Option<&'static CmdSpec> {
    let name = std::str::from_utf8(name).ok()?;
    let (top, _) = name.split_once('|')?;
    spec(top.as_bytes())?.subs.iter().find(|s| s.name.eq_ignore_ascii_case(name))
}

pub(crate) fn encode_sub(out: &mut Vec<u8>, s: &CmdSpec, proto: RespVersion) {
    encode_spec_row(out, s, s.arity, proto);
}
