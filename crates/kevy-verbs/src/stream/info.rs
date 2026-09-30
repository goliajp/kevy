//! `XINFO STREAM | GROUPS | CONSUMERS | HELP`, read-only over
//! [`Store::stream_view`]. A reply that is a list of named fields is an
//! array of name/value pairs under RESP2 and a map under RESP3; a missing
//! value is a nil bulk under RESP2 and a null under RESP3.

use kevy_resp::{
    ArgvView, RespVersion, encode_array_len, encode_bulk, encode_error, encode_integer,
    encode_map_header, encode_null, encode_null_bulk, encode_simple_string,
};
use kevy_store::{ConsumerGroup, ConsumerState, Store, StreamData, StreamId, now_unix_ms};

use crate::reply::{store_err, wrong_args};

#[path = "info_full.rs"]
mod full;

/// `XINFO` with the reply shapes of `proto`.
///
/// ```
/// use kevy_resp::{Argv, RespVersion};
/// if kevy_verbs::verb(b"XINFO").is_none() {
///     return; // built without the `streams-geo` feature
/// }
/// let argv = |s: &str| Argv::from(s.split(' ').map(|p| p.as_bytes().to_vec()).collect::<Vec<_>>());
/// let mut store = kevy_store::Store::new();
/// kevy_verbs::exec(&mut store, b"XGROUP", &argv("XGROUP CREATE s g $ MKSTREAM"), &mut Vec::new());
/// let mut out = Vec::new();
/// kevy_verbs::cmd::xinfo(&mut store, &argv("XINFO GROUPS s"), &mut out, RespVersion::V3);
/// assert!(out.starts_with(b"*1\r\n%6\r\n"), "one group, as a map of six fields");
/// ```
pub fn xinfo<A: ArgvView + ?Sized>(
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
    proto: RespVersion,
) {
    if args.len() < 2 {
        return wrong_args(out, "xinfo");
    }
    let sub = args[1].to_ascii_uppercase();
    match sub.as_slice() {
        b"STREAM" => xinfo_stream(store, args, out, proto),
        b"GROUPS" => xinfo_groups(store, args, out, proto),
        b"CONSUMERS" => xinfo_consumers(store, args, out, proto),
        b"HELP" if args.len() == 2 => xinfo_help(out),
        b"HELP" => wrong_args(out, "xinfo|help"),
        _ => encode_error(
            out,
            &format!(
                "ERR unknown subcommand '{}'. Try XINFO HELP.",
                String::from_utf8_lossy(&args[1]),
            ),
        ),
    }
}

/// The stream at `key`, or the error reply already written.
fn stream<'s>(store: &'s mut Store, key: &[u8], out: &mut Vec<u8>) -> Option<&'s StreamData> {
    match store.stream_view(key) {
        Ok(Some(s)) => Some(s),
        Ok(None) => {
            encode_error(out, "ERR no such key");
            None
        }
        Err(e) => {
            store_err(out, e);
            None
        }
    }
}

fn xinfo_stream<A: ArgvView + ?Sized>(
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
    proto: RespVersion,
) {
    if args.len() < 3 {
        return wrong_args(out, "xinfo|stream");
    }
    let Some(s) = stream(store, &args[2], out) else { return };
    match args.len() {
        3 => emit_stream(out, s, proto),
        4 if args[3].eq_ignore_ascii_case(b"FULL") => full::emit(out, s, 10, proto),
        6 if args[3].eq_ignore_ascii_case(b"FULL") && args[4].eq_ignore_ascii_case(b"COUNT") => {
            match crate::args::arg_i64(&args[5]) {
                // zero or less shows everything
                Some(n) => full::emit(out, s, usize::try_from(n).unwrap_or(0), proto),
                None => encode_error(out, crate::reply::ERR_NOT_INT),
            }
        }
        _ => encode_error(
            out,
            &format!(
                "ERR unknown subcommand or wrong number of arguments for '{}'. Try XINFO HELP.",
                String::from_utf8_lossy(&args[1]),
            ),
        ),
    }
}

/// The head every form of `XINFO STREAM` opens with: seven pairs.
fn emit_stream_head(out: &mut Vec<u8>, s: &StreamData) {
    field(out, "length");
    encode_integer(out, s.length() as i64);
    field(out, "radix-tree-keys");
    encode_integer(out, s.node_count() as i64);
    field(out, "radix-tree-nodes");
    encode_integer(out, s.radix_tree_nodes() as i64);
    field(out, "last-generated-id");
    encode_id(out, s.last_id());
    field(out, "max-deleted-entry-id");
    encode_id(out, s.max_deleted_id());
    field(out, "entries-added");
    encode_integer(out, s.entries_added() as i64);
    field(out, "recorded-first-entry-id");
    encode_id(out, s.first_entry().map_or(StreamId::MIN, |(id, _)| id));
}

fn emit_stream(out: &mut Vec<u8>, s: &StreamData, proto: RespVersion) {
    map_header(out, proto, 10);
    emit_stream_head(out, s);
    field(out, "groups");
    encode_integer(out, s.group_count() as i64);
    field(out, "first-entry");
    match s.first_entry() {
        Some((id, fv)) => emit_entry(out, id, fv),
        None => nil(out, proto),
    }
    field(out, "last-entry");
    match s.last_entry() {
        Some((id, fv)) => emit_entry(out, id, fv),
        None => nil(out, proto),
    }
}

fn xinfo_groups<A: ArgvView + ?Sized>(
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
    proto: RespVersion,
) {
    if args.len() != 3 {
        return wrong_args(out, "xinfo|groups");
    }
    let Some(s) = stream(store, &args[2], out) else { return };
    let groups = sorted_groups(s);
    encode_array_len(out, groups.len() as i64);
    for (name, g) in groups {
        map_header(out, proto, 6);
        field(out, "name");
        encode_bulk(out, name);
        field(out, "consumers");
        encode_integer(out, g.consumer_count() as i64);
        field(out, "pending");
        encode_integer(out, g.pending_count() as i64);
        field(out, "last-delivered-id");
        encode_id(out, g.last_delivered_id());
        emit_read_and_lag(out, s, g, proto);
    }
}

/// `entries-read` then `lag`, each nil when unknown.
fn emit_read_and_lag(out: &mut Vec<u8>, s: &StreamData, g: &ConsumerGroup, proto: RespVersion) {
    field(out, "entries-read");
    match g.entries_read() {
        Some(n) => encode_integer(out, i64::try_from(n).unwrap_or(i64::MAX)),
        None => nil(out, proto),
    }
    field(out, "lag");
    match s.group_lag(g) {
        Some(n) => encode_integer(out, n),
        None => nil(out, proto),
    }
}

fn xinfo_consumers<A: ArgvView + ?Sized>(
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
    proto: RespVersion,
) {
    if args.len() != 4 {
        return wrong_args(out, "xinfo|consumers");
    }
    let Some(s) = stream(store, &args[2], out) else { return };
    let Some(g) = s.group(&args[3]) else {
        return no_group(out, &args[2], &args[3]);
    };
    let consumers = sorted_consumers(g);
    encode_array_len(out, consumers.len() as i64);
    let now = now_unix_ms();
    for (name, c) in consumers {
        map_header(out, proto, 4);
        field(out, "name");
        encode_bulk(out, name);
        field(out, "pending");
        encode_integer(out, c.pending_count() as i64);
        field(out, "idle");
        encode_integer(out, since(now, c.last_seen_ms()));
        field(out, "inactive");
        encode_integer(out, c.last_active_ms().map_or(-1, |at| since(now, at)));
    }
}

/// The `NOGROUP` reply naming the group and the key.
pub(super) fn no_group(out: &mut Vec<u8>, key: &[u8], group: &[u8]) {
    encode_error(
        out,
        &format!(
            "NOGROUP No such consumer group '{}' for key name '{}'",
            String::from_utf8_lossy(group),
            String::from_utf8_lossy(key),
        ),
    );
}

fn xinfo_help(out: &mut Vec<u8>) {
    const LINES: [&str; 9] = [
        "XINFO <subcommand> [<arg> [value] [opt] ...]. Subcommands are:",
        "CONSUMERS <key> <groupname>",
        "    Show consumers of <groupname>.",
        "GROUPS <key>",
        "    Show the stream consumer groups.",
        "STREAM <key> [FULL [COUNT <count>]",
        "    Show information about the stream.",
        "HELP",
        "    Print this help.",
    ];
    encode_array_len(out, LINES.len() as i64);
    for line in LINES {
        encode_simple_string(out, line);
    }
}

/// Milliseconds from `at` to `now`, never negative.
fn since(now: u64, at: u64) -> i64 {
    i64::try_from(now.saturating_sub(at)).unwrap_or(i64::MAX)
}

/// The groups by name, in byte order, as a Redis server lists them.
fn sorted_groups(s: &StreamData) -> Vec<(&[u8], &ConsumerGroup)> {
    let mut groups: Vec<(&[u8], &ConsumerGroup)> = s.groups().collect();
    groups.sort_unstable_by_key(|(name, _)| *name);
    groups
}

/// The consumers by name, in byte order.
fn sorted_consumers(g: &ConsumerGroup) -> Vec<(&[u8], &ConsumerState)> {
    let mut consumers: Vec<(&[u8], &ConsumerState)> = g.consumers().collect();
    consumers.sort_unstable_by_key(|(name, _)| *name);
    consumers
}

/// A header for `pairs` named fields: pairs flattened under RESP2, a map
/// under RESP3.
fn map_header(out: &mut Vec<u8>, proto: RespVersion, pairs: i64) {
    match proto {
        RespVersion::V2 => encode_array_len(out, pairs * 2),
        RespVersion::V3 => encode_map_header(out, pairs),
    }
}

fn nil(out: &mut Vec<u8>, proto: RespVersion) {
    match proto {
        RespVersion::V2 => encode_null_bulk(out),
        RespVersion::V3 => encode_null(out),
    }
}

fn field(out: &mut Vec<u8>, name: &str) {
    encode_bulk(out, name.as_bytes());
}

fn encode_id(out: &mut Vec<u8>, id: StreamId) {
    let mut buf = [0u8; 41];
    encode_bulk(out, crate::aof::id_bytes(&mut buf, id));
}

fn emit_entry(
    out: &mut Vec<u8>,
    id: StreamId,
    fv: &[(kevy_store::SmallBytes, kevy_store::SmallBytes)],
) {
    encode_array_len(out, 2);
    encode_id(out, id);
    encode_array_len(out, (fv.len() * 2) as i64);
    for (f, v) in fv {
        encode_bulk(out, f.as_slice());
        encode_bulk(out, v.as_slice());
    }
}
