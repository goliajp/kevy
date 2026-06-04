//! `XINFO STREAM | GROUPS | CONSUMERS | HELP` — sprint C of v2-7.
//! Pure read-side dispatch on top of [`Store::stream_view`]; no new
//! state in the keyspace.

use kevy_resp::{
    ArgvView, encode_array_len, encode_bulk, encode_error, encode_integer, encode_null_bulk,
    encode_simple_string,
};
use kevy_store::{ConsumerGroup, ConsumerState, Store, StreamData, now_unix_ms};

use crate::cmd::{store_err, wrong_args};

pub(super) fn cmd_xinfo<A: ArgvView + ?Sized>(
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
) {
    if args.len() < 2 {
        return wrong_args(out, "xinfo");
    }
    let sub = args[1].to_ascii_uppercase();
    match sub.as_slice() {
        b"STREAM" => xinfo_stream(store, args, out),
        b"GROUPS" => xinfo_groups(store, args, out),
        b"CONSUMERS" => xinfo_consumers(store, args, out),
        b"HELP" => xinfo_help(out),
        other => encode_error(
            out,
            &format!(
                "ERR Unknown XINFO subcommand or wrong number of arguments for '{}'",
                String::from_utf8_lossy(other),
            ),
        ),
    }
}

fn xinfo_stream<A: ArgvView + ?Sized>(store: &mut Store, args: &A, out: &mut Vec<u8>) {
    if args.len() < 3 {
        return wrong_args(out, "xinfo|stream");
    }
    let s = match store.stream_view(&args[2]) {
        Ok(Some(s)) => s,
        Ok(None) => return encode_error(out, "ERR no such key"),
        Err(e) => return store_err(out, e),
    };
    emit_stream_info(out, s);
}

fn emit_stream_info(out: &mut Vec<u8>, s: &StreamData) {
    // 10 key-value pairs = 20 array entries.
    encode_array_len(out, 20);
    field(out, "length");
    encode_integer(out, s.length() as i64);
    field(out, "last-generated-id");
    encode_bulk(out, &s.last_id().encode());
    field(out, "max-deleted-entry-id");
    encode_bulk(out, &s.max_deleted_id().encode());
    field(out, "entries-added");
    encode_integer(out, s.entries_added() as i64);
    field(out, "recorded-first-entry-id");
    match s.first_entry() {
        Some((id, _)) => encode_bulk(out, &id.encode()),
        None => encode_bulk(out, b"0-0"),
    }
    field(out, "groups");
    encode_integer(out, s.group_count() as i64);
    field(out, "first-entry");
    match s.first_entry() {
        Some((id, fv)) => emit_entry(out, id, fv),
        None => encode_null_bulk(out),
    }
    field(out, "last-entry");
    match s.last_entry() {
        Some((id, fv)) => emit_entry(out, id, fv),
        None => encode_null_bulk(out),
    }
    // Two filler stats so legacy redis-cli STREAM dumps round-trip
    // without complaint. We don't carry a radix tree internally
    // (BTreeMap), so the numbers are best-effort.
    field(out, "radix-tree-keys");
    encode_integer(out, s.length() as i64);
    field(out, "radix-tree-nodes");
    encode_integer(out, 1);
}

fn xinfo_groups<A: ArgvView + ?Sized>(store: &mut Store, args: &A, out: &mut Vec<u8>) {
    if args.len() != 3 {
        return wrong_args(out, "xinfo|groups");
    }
    let s = match store.stream_view(&args[2]) {
        Ok(Some(s)) => s,
        Ok(None) => return encode_error(out, "ERR no such key"),
        Err(e) => return store_err(out, e),
    };
    let groups: Vec<(&[u8], &ConsumerGroup)> = s.groups_iter().collect();
    encode_array_len(out, groups.len() as i64);
    for (name, g) in groups {
        emit_group_info(out, name, g, s);
    }
}

fn emit_group_info(out: &mut Vec<u8>, name: &[u8], g: &ConsumerGroup, s: &StreamData) {
    // 6 key-value pairs = 12 entries.
    encode_array_len(out, 12);
    field(out, "name");
    encode_bulk(out, name);
    field(out, "consumers");
    encode_integer(out, g.consumer_count() as i64);
    field(out, "pending");
    encode_integer(out, g.pending_count() as i64);
    field(out, "last-delivered-id");
    encode_bulk(out, &g.last_delivered_id().encode());
    // `lag` = number of entries strictly between last_delivered_id and
    // the stream's last_id. Best-effort — undeleted-stream answer.
    let lag = entries_between(s, g.last_delivered_id());
    field(out, "lag");
    encode_integer(out, lag);
    // `entries-read` is not tracked at byte-for-byte fidelity — we
    // approximate as PEL + ACKed-so-far ≈ stream length minus lag.
    field(out, "entries-read");
    encode_integer(out, (s.length() as i64 - lag).max(0));
}

fn entries_between(s: &StreamData, last_delivered_id: kevy_store::StreamId) -> i64 {
    let mut n = 0i64;
    for (id, _) in s
        .iter_entries()
        .skip_while(|(id, _)| *id <= last_delivered_id)
    {
        let _ = id;
        n += 1;
    }
    n
}

fn xinfo_consumers<A: ArgvView + ?Sized>(store: &mut Store, args: &A, out: &mut Vec<u8>) {
    if args.len() != 4 {
        return wrong_args(out, "xinfo|consumers");
    }
    let s = match store.stream_view(&args[2]) {
        Ok(Some(s)) => s,
        Ok(None) => return encode_error(out, "ERR no such key"),
        Err(e) => return store_err(out, e),
    };
    let Some(g) = s.group(&args[3]) else {
        return encode_error(out, "NOGROUP No such consumer group");
    };
    let consumers: Vec<(&[u8], &ConsumerState)> = g.consumers_iter().collect();
    encode_array_len(out, consumers.len() as i64);
    let now = now_unix_ms();
    for (name, c) in consumers {
        emit_consumer_info(out, name, c, now);
    }
}

fn emit_consumer_info(out: &mut Vec<u8>, name: &[u8], c: &ConsumerState, now_ms: u64) {
    // 3 key-value pairs = 6 entries.
    encode_array_len(out, 6);
    field(out, "name");
    encode_bulk(out, name);
    field(out, "pending");
    encode_integer(out, c.pending_count() as i64);
    field(out, "idle");
    let idle = now_ms.saturating_sub(c.last_seen_ms());
    encode_integer(out, idle as i64);
}

fn xinfo_help(out: &mut Vec<u8>) {
    let lines: &[&[u8]] = &[
        b"XINFO STREAM <key>",
        b"XINFO GROUPS <key>",
        b"XINFO CONSUMERS <key> <group>",
        b"XINFO HELP",
    ];
    encode_array_len(out, lines.len() as i64);
    for line in lines {
        encode_simple_string(out, std::str::from_utf8(line).unwrap_or(""));
    }
}

fn field(out: &mut Vec<u8>, name: &str) {
    encode_bulk(out, name.as_bytes());
}

fn emit_entry(
    out: &mut Vec<u8>,
    id: kevy_store::StreamId,
    fv: &[(kevy_store::SmallBytes, kevy_store::SmallBytes)],
) {
    encode_array_len(out, 2);
    encode_bulk(out, &id.encode());
    encode_array_len(out, (fv.len() * 2) as i64);
    for (f, v) in fv {
        encode_bulk(out, f.as_slice());
        encode_bulk(out, v.as_slice());
    }
}
