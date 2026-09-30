//! `XGROUP CREATE | DESTROY | SETID | CREATECONSUMER | DELCONSUMER`.
//! `CREATE` and `SETID` take `ENTRIESREAD n`, the group's read counter
//! (`-1` = unknown); without it the counter is unknown.

use kevy_resp::CmdError;
use kevy_resp::{ArgvView, encode_error, encode_integer, encode_simple_string};
use kevy_store::{GroupCreateMode, MissingStream, Store, now_unix_ms, parse_explicit_id};

use crate::Effect;
use crate::reply::{store_err, wrong_args};

const NO_KEY: &str = "ERR The XGROUP subcommand requires the key to exist. \
     Note that for CREATE you may want to use the MKSTREAM option to create an empty stream automatically.";

pub(super) fn cmd_xgroup<A: ArgvView + ?Sized>(
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
) -> Effect {
    if args.len() < 2 {
        wrong_args(out, "xgroup");
        return Effect::Write;
    }
    let sub = args[1].to_ascii_uppercase();
    match sub.as_slice() {
        b"CREATE" => xgroup_create(store, args, out),
        b"DESTROY" => xgroup_destroy(store, args, out),
        b"SETID" => xgroup_setid(store, args, out),
        b"CREATECONSUMER" => return xgroup_create_consumer(store, args, out),
        b"DELCONSUMER" => xgroup_del_consumer(store, args, out),
        b"HELP" if args.len() == 2 => xgroup_help(out),
        b"HELP" => wrong_args(out, "xgroup|help"),
        _ => encode_error(
            out,
            &format!(
                "ERR unknown subcommand '{}'. Try XGROUP HELP.",
                String::from_utf8_lossy(&args[1]),
            ),
        ),
    }
    Effect::Write
}

fn xgroup_help(out: &mut Vec<u8>) {
    const LINES: [&str; 17] = [
        "XGROUP <subcommand> [<arg> [value] [opt] ...]. Subcommands are:",
        "CREATE <key> <groupname> <id|$> [option]",
        "    Create a new consumer group. Options are:",
        "    * MKSTREAM",
        "      Create the empty stream if it does not exist.",
        "    * ENTRIESREAD entries_read",
        "      Set the group's entries_read counter (internal use).",
        "CREATECONSUMER <key> <groupname> <consumer>",
        "    Create a new consumer in the specified group.",
        "DELCONSUMER <key> <groupname> <consumer>",
        "    Remove the specified consumer.",
        "DESTROY <key> <groupname>",
        "    Remove the specified group.",
        "SETID <key> <groupname> <id|$> [ENTRIESREAD entries_read]",
        "    Set the current group ID and entries_read counter.",
        "HELP",
        "    Print this help.",
    ];
    kevy_resp::encode_array_len(out, LINES.len() as i64);
    for line in LINES {
        encode_simple_string(out, line);
    }
}

/// The key must hold a stream, and — when `group` is `Some` — the group
/// must exist; otherwise the refusal is written to `out`.
fn has_group(store: &mut Store, key: &[u8], group: Option<&[u8]>, out: &mut Vec<u8>) -> bool {
    match store.stream_view(key) {
        Ok(Some(s)) => match group {
            Some(g) if s.group(g).is_none() => {
                super::info::no_group(out, key, g);
                false
            }
            _ => true,
        },
        Ok(None) => {
            encode_error(out, NO_KEY);
            false
        }
        Err(e) => {
            store_err(out, e);
            false
        }
    }
}

/// The options after `CREATE key group id` / `SETID key group id`.
struct GroupOpts {
    mkstream: bool,
    /// `Some(None)` = `ENTRIESREAD -1`.
    entries_read: Option<Option<u64>>,
}

/// Parse the options from `args[5]` on, `MKSTREAM` only where
/// `mkstream_ok`, writing the refusal to `out` on `None`. Each
/// `ENTRIESREAD` value is checked as it is met, before a repeat is
/// refused.
fn parse_group_opts<A: ArgvView + ?Sized>(
    args: &A,
    mkstream_ok: bool,
    out: &mut Vec<u8>,
) -> Option<GroupOpts> {
    let mut opts = GroupOpts { mkstream: false, entries_read: None };
    let mut i = 5;
    while i < args.len() {
        let tok = &args[i];
        if mkstream_ok && tok.eq_ignore_ascii_case(b"MKSTREAM") {
            opts.mkstream = true;
            i += 1;
            continue;
        }
        if !tok.eq_ignore_ascii_case(b"ENTRIESREAD") || i + 1 == args.len() {
            return bad_options(args, out);
        }
        let n = match crate::args::arg_i64(&args[i + 1]) {
            Some(-1) => None,
            Some(n) if n >= 0 => Some(n as u64),
            Some(_) => {
                encode_error(out, "ERR value for ENTRIESREAD must be positive or -1");
                return None;
            }
            None => {
                encode_error(out, crate::reply::ERR_NOT_INT);
                return None;
            }
        };
        if opts.entries_read.replace(n).is_some() {
            return bad_options(args, out);
        }
        i += 2;
    }
    Some(opts)
}

fn bad_options<A: ArgvView + ?Sized>(args: &A, out: &mut Vec<u8>) -> Option<GroupOpts> {
    encode_error(
        out,
        &format!(
            "ERR unknown subcommand or wrong number of arguments for '{}'. Try XGROUP HELP.",
            String::from_utf8_lossy(&args[1]),
        ),
    );
    None
}

/// `XGROUP CREATE key group <id|$> [MKSTREAM] [ENTRIESREAD n]`
fn xgroup_create<A: ArgvView + ?Sized>(store: &mut Store, args: &A, out: &mut Vec<u8>) {
    if args.len() < 5 {
        return wrong_args(out, "xgroup|create");
    }
    let (key, group) = (&args[2], &args[3]);
    let Some(opts) = parse_group_opts(args, true, out) else { return };
    match store.stream_view(key) {
        Ok(None) if !opts.mkstream => return encode_error(out, NO_KEY),
        Err(e) => return store_err(out, e),
        Ok(_) => {}
    }
    let mode = match parse_id_or_dollar(&args[4]) {
        Ok(m) => m,
        Err(msg) => return encode_error(out, msg.as_wire()),
    };
    let missing = if opts.mkstream { MissingStream::Create } else { MissingStream::Refuse };
    match store.xgroup_create(key, group, mode, missing) {
        Ok(true) => {
            if let Some(n) = opts.entries_read {
                let _ = store.xgroup_set_entries_read(key, group, n);
            }
            encode_simple_string(out, "OK");
        }
        Ok(false) => encode_error(out, "BUSYGROUP Consumer Group name already exists"),
        Err(kevy_store::StoreError::NoSuchKey) => encode_error(out, NO_KEY),
        Err(e) => store_err(out, e),
    }
}

fn xgroup_destroy<A: ArgvView + ?Sized>(store: &mut Store, args: &A, out: &mut Vec<u8>) {
    if args.len() != 4 {
        return wrong_args(out, "xgroup|destroy");
    }
    if !has_group(store, &args[2], None, out) {
        return;
    }
    match store.xgroup_destroy(&args[2], &args[3]) {
        Ok(true) => encode_integer(out, 1),
        Ok(false) => encode_integer(out, 0),
        Err(e) => store_err(out, e),
    }
}

/// `XGROUP SETID key group <id|$> [ENTRIESREAD n]`: the key and the group
/// are looked for before the ID is read.
fn xgroup_setid<A: ArgvView + ?Sized>(store: &mut Store, args: &A, out: &mut Vec<u8>) {
    if args.len() < 5 {
        return wrong_args(out, "xgroup|setid");
    }
    let (key, group) = (&args[2], &args[3]);
    let Some(opts) = parse_group_opts(args, false, out) else { return };
    if !has_group(store, key, Some(group), out) {
        return;
    }
    let mode = match parse_id_or_dollar(&args[4]) {
        Ok(m) => m,
        Err(msg) => return encode_error(out, msg.as_wire()),
    };
    match store.xgroup_setid(key, group, mode) {
        Ok(_) => {
            if let Some(n) = opts.entries_read {
                let _ = store.xgroup_set_entries_read(key, group, n);
            }
            encode_simple_string(out, "OK");
        }
        Err(e) => store_err(out, e),
    }
}

/// `XGROUP CREATECONSUMER key group consumer`. A consumer this creates
/// is recorded with the time it was created at ([`Effect::RecordSeen`]).
fn xgroup_create_consumer<A: ArgvView + ?Sized>(
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
) -> Effect {
    if args.len() != 5 {
        wrong_args(out, "xgroup|createconsumer");
        return Effect::Write;
    }
    if !has_group(store, &args[2], Some(&args[3]), out) {
        return Effect::Write;
    }
    match store.xgroup_create_consumer(&args[2], &args[3], &args[4], now_unix_ms()) {
        Ok(true) => {
            encode_integer(out, 1);
            Effect::RecordSeen
        }
        Ok(false) => {
            encode_integer(out, 0);
            Effect::Unchanged
        }
        Err(e) => {
            store_err(out, e);
            Effect::Write
        }
    }
}

fn xgroup_del_consumer<A: ArgvView + ?Sized>(store: &mut Store, args: &A, out: &mut Vec<u8>) {
    if args.len() != 5 {
        return wrong_args(out, "xgroup|delconsumer");
    }
    if !has_group(store, &args[2], Some(&args[3]), out) {
        return;
    }
    match store.xgroup_del_consumer(&args[2], &args[3], &args[4]) {
        Ok(n) => encode_integer(out, n as i64),
        Err(e) => store_err(out, e),
    }
}

fn parse_id_or_dollar(s: &[u8]) -> Result<GroupCreateMode, CmdError> {
    if s == b"$" {
        return Ok(GroupCreateMode::AtCurrent);
    }
    parse_explicit_id(s)
        .map(GroupCreateMode::AtId)
        .map_err(|_| CmdError::Wire("ERR Invalid stream ID specified as stream command argument"))
}
