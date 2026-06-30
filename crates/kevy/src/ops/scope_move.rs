//! `MOVE-SCOPE` + `MOVE-SCOPE-INGEST` — scope migration operator
//! commands.
//!
//! Q3=(a) quiesce-window mechanism per the RFC `## Q3 resolution`.
//! Operator runs `MOVE-SCOPE <prefix> FROM <from-id> TO <to-id>`
//! against the source writer. The writer:
//!
//! 1. Validates: self is `<from-id>`, `<to-id>` resolves to a
//!    `host:port` in the peer table.
//! 2. Flips the local migration state to MIGRATING; subsequent
//!    writes for the prefix return `-QUIESCED migrating to
//!    <to-host:port>` (wired by T3.14 routing).
//! 3. Serializes the prefix's keyspace slice (5 data types: string
//!    / hash / list / set / zset; TTLs as absolute `PEXPIREAT`).
//! 4. Connects to the target's data port and sends one
//!    `MOVE-SCOPE-INGEST <prefix> <bulk>` command.
//! 5. On `+OK`, commits the migration locally; future writes for
//!    the prefix on the source return `-MISDIRECTED writer is
//!    <to-host:port>` (no quiesce — move done).
//! 6. On error, aborts the migration; writes for the prefix on
//!    the source resume.
//!
//! Target side handler bypasses scope routing during the ingest
//! window via a thread-local guard, then dispatches each embedded
//! command normally through `kevy::dispatch_into`.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use kevy_resp::{ArgvView, encode_error, parse_command};
use kevy_store::Store;

use crate::scope_integration;

/// `MOVE-SCOPE <prefix> FROM <from-id> TO <to-id>` — operator-issued
/// scope migration.
pub(crate) fn cmd_move_scope<A: ArgvView + ?Sized>(
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
) {
    if args.len() != 6 {
        return encode_error(
            out,
            "ERR wrong number of arguments — MOVE-SCOPE <prefix> FROM <from-id> TO <to-id>",
        );
    }
    let Some(prefix) = args.get(1) else { return wrong_syntax(out) };
    let from_kw = args.get(2).unwrap_or_default();
    let from_id = args.get(3).unwrap_or_default();
    let to_kw = args.get(4).unwrap_or_default();
    let to_id = args.get(5).unwrap_or_default();
    if !from_kw.eq_ignore_ascii_case(b"FROM") || !to_kw.eq_ignore_ascii_case(b"TO") {
        return wrong_syntax(out);
    }
    let Ok(from_id) = std::str::from_utf8(from_id) else { return wrong_syntax(out) };
    let Ok(to_id) = std::str::from_utf8(to_id) else { return wrong_syntax(out) };
    let prefix_owned = prefix.to_vec();

    // Self must be the source. Local writes flow only through this
    // node's keyspace; a misdirected MOVE-SCOPE would silently lose
    // half the data.
    match scope_integration::self_node_id() {
        Some(me) if me == from_id => {}
        Some(me) => {
            return encode_error(
                out,
                &format!("ERR MOVE-SCOPE: from-id {from_id:?} is not this node ({me:?})"),
            );
        }
        None => {
            return encode_error(
                out,
                "ERR MOVE-SCOPE: [cluster] node_id is not configured on this node",
            );
        }
    }

    let Some(target_addr) = scope_integration::peer_addr(to_id) else {
        return encode_error(
            out,
            &format!("ERR MOVE-SCOPE: target node {to_id:?} not in [cluster] peers"),
        );
    };

    // Start the migration locally. From this instant, dispatch
    // routes writes for this prefix to `-QUIESCED migrating to
    // <to_addr>` (T3.14).
    if let Err(e) = scope_integration::migration_start(
        prefix_owned.clone(),
        from_id.to_string(),
        to_id.to_string(),
    ) {
        return encode_error(out, &format!("ERR MOVE-SCOPE: {e}"));
    }

    // Ship.
    match ship_prefix_to_target(store, &prefix_owned, &target_addr) {
        Ok(count) => {
            scope_integration::migration_commit(&prefix_owned);
            let reply = format!("+OK {count}\r\n");
            out.extend_from_slice(reply.as_bytes());
        }
        Err(e) => {
            scope_integration::migration_abort(&prefix_owned);
            encode_error(out, &format!("ERR MOVE-SCOPE ship failed: {e}"));
        }
    }
}

fn wrong_syntax(out: &mut Vec<u8>) {
    encode_error(
        out,
        "ERR MOVE-SCOPE syntax: MOVE-SCOPE <prefix> FROM <from-id> TO <to-id>",
    );
}

/// Walk the local keyspace, reconstruct keys matching `prefix` as
/// RESP frames, send via one `MOVE-SCOPE-INGEST <prefix> <bulk>` to
/// `target_addr`. Returns the number of reconstruction commands
/// emitted (not the number of distinct keys — each key needs ≥ 1).
fn ship_prefix_to_target(
    store: &mut Store,
    prefix: &[u8],
    target_addr: &str,
) -> Result<usize, String> {
    let (bulk, count) = serialize_prefix(store, prefix);

    let mut s = TcpStream::connect_timeout(
        &target_addr.parse().map_err(|e| format!("bad target addr {target_addr:?}: {e}"))?,
        Duration::from_secs(10),
    )
    .map_err(|e| format!("connect {target_addr:?}: {e}"))?;
    s.set_read_timeout(Some(Duration::from_secs(60)))
        .map_err(|e| format!("set_read_timeout: {e}"))?;

    let mut req = Vec::new();
    req.extend_from_slice(b"*3\r\n");
    req.extend_from_slice(b"$17\r\nMOVE-SCOPE-INGEST\r\n");
    req.extend_from_slice(format!("${}\r\n", prefix.len()).as_bytes());
    req.extend_from_slice(prefix);
    req.extend_from_slice(b"\r\n");
    req.extend_from_slice(format!("${}\r\n", bulk.len()).as_bytes());
    req.extend_from_slice(&bulk);
    req.extend_from_slice(b"\r\n");

    s.write_all(&req).map_err(|e| format!("write: {e}"))?;

    // Read enough of the reply to confirm `+OK ...`. We trust the
    // target to send a single response line for this command.
    let mut buf = [0u8; 256];
    let n = s.read(&mut buf).map_err(|e| format!("read: {e}"))?;
    let reply = &buf[..n];
    if !reply.starts_with(b"+") {
        return Err(format!(
            "target replied non-OK: {:?}",
            String::from_utf8_lossy(reply)
        ));
    }
    Ok(count)
}

/// `MOVE-SCOPE-INGEST <prefix> <bulk>` — target-side receiver.
/// Parses concatenated RESP commands out of `<bulk>` and dispatches
/// each one with scope routing bypassed for `<prefix>`.
pub(crate) fn cmd_move_scope_ingest<A: ArgvView + ?Sized>(
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
) {
    if args.len() != 3 {
        return encode_error(
            out,
            "ERR wrong number of arguments — MOVE-SCOPE-INGEST <prefix> <bulk>",
        );
    }
    let Some(prefix) = args.get(1) else {
        return encode_error(out, "ERR MOVE-SCOPE-INGEST: missing prefix");
    };
    let Some(bulk) = args.get(2) else {
        return encode_error(out, "ERR MOVE-SCOPE-INGEST: missing bulk");
    };

    let _guard = scope_integration::IngestGuard::enter(prefix.to_vec());
    let mut buf = bulk.to_vec();
    let mut applied = 0usize;
    let mut scratch = Vec::with_capacity(256);
    loop {
        match parse_command(&buf) {
            Ok(Some((argv, consumed))) => {
                scratch.clear();
                crate::dispatch::dispatch_into(store, &argv, &mut scratch);
                buf.drain(..consumed);
                applied += 1;
            }
            Ok(None) => break,
            Err(_) => {
                return encode_error(out, "ERR MOVE-SCOPE-INGEST: malformed bulk");
            }
        }
    }
    let reply = format!("+OK {applied}\r\n");
    out.extend_from_slice(reply.as_bytes());
}

/// Walk `store`, collect every key matching `prefix`, reconstruct
/// each as one (or two — for TTL'd keys) RESP frame. Returns the
/// concatenated wire bytes + the frame count.
fn serialize_prefix(store: &mut Store, prefix: &[u8]) -> (Vec<u8>, usize) {
    let mut bulk = Vec::new();
    let mut count = 0usize;
    let keys = store.collect_keys(None, None);
    for key in keys {
        if !key.starts_with(prefix) {
            continue;
        }
        let ttl_ms = store.pttl(&key);
        let abs_expire = if ttl_ms > 0 {
            Some(kevy_store::now_unix_ms().saturating_add(ttl_ms as u64))
        } else {
            None
        };
        match store.type_of(&key) {
            "string" => emit_string(store, &key, &mut bulk, &mut count),
            "hash" => emit_hash(store, &key, &mut bulk, &mut count),
            "list" => emit_list(store, &key, &mut bulk, &mut count),
            "set" => emit_set(store, &key, &mut bulk, &mut count),
            "zset" => emit_zset(store, &key, &mut bulk, &mut count),
            _ => continue, // stream / none — v1.21 skips streams (TODO)
        }
        if let Some(ms) = abs_expire {
            let ms_str = ms.to_string();
            append_resp_argv(&mut bulk, &[b"PEXPIREAT", &key, ms_str.as_bytes()]);
            count += 1;
        }
    }
    (bulk, count)
}

fn emit_string(store: &mut Store, key: &[u8], bulk: &mut Vec<u8>, count: &mut usize) {
    if let Ok(Some(v)) = store.get(key) {
        append_resp_argv(bulk, &[b"SET", key, &v]);
        *count += 1;
    }
}

fn emit_hash(store: &mut Store, key: &[u8], bulk: &mut Vec<u8>, count: &mut usize) {
    let Ok(pairs) = store.hgetall(key) else { return };
    if pairs.is_empty() {
        return;
    }
    let mut parts: Vec<&[u8]> = Vec::with_capacity(2 + pairs.len());
    parts.push(b"HSET");
    parts.push(key);
    for p in &pairs {
        parts.push(p);
    }
    append_resp_argv(bulk, &parts);
    *count += 1;
}

fn emit_list(store: &mut Store, key: &[u8], bulk: &mut Vec<u8>, count: &mut usize) {
    let Ok(items) = store.lrange(key, 0, -1) else { return };
    if items.is_empty() {
        return;
    }
    let mut parts: Vec<&[u8]> = Vec::with_capacity(2 + items.len());
    parts.push(b"RPUSH");
    parts.push(key);
    for item in &items {
        parts.push(item);
    }
    append_resp_argv(bulk, &parts);
    *count += 1;
}

fn emit_set(store: &mut Store, key: &[u8], bulk: &mut Vec<u8>, count: &mut usize) {
    let Ok(members) = store.smembers(key) else { return };
    if members.is_empty() {
        return;
    }
    let mut parts: Vec<&[u8]> = Vec::with_capacity(2 + members.len());
    parts.push(b"SADD");
    parts.push(key);
    for m in &members {
        parts.push(m);
    }
    append_resp_argv(bulk, &parts);
    *count += 1;
}

fn emit_zset(store: &mut Store, key: &[u8], bulk: &mut Vec<u8>, count: &mut usize) {
    let Ok(items) = store.zrange(key, 0, -1) else { return };
    if items.is_empty() {
        return;
    }
    // ZADD key score1 member1 score2 member2 ...
    // Score strings owned in a Vec so we can borrow as &[u8] for parts.
    let score_strs: Vec<String> = items.iter().map(|(_, s)| format_score(*s)).collect();
    let mut parts: Vec<&[u8]> = Vec::with_capacity(2 + items.len() * 2);
    parts.push(b"ZADD");
    parts.push(key);
    for (i, (member, _)) in items.iter().enumerate() {
        parts.push(score_strs[i].as_bytes());
        parts.push(member);
    }
    append_resp_argv(bulk, &parts);
    *count += 1;
}

fn format_score(s: f64) -> String {
    // Match the wire shape kevy_resp uses for doubles — finite as
    // shortest decimal, NaN/inf rejected upstream so we don't see
    // them here. `{s}` (Display) on f64 already gives the right
    // round-trip representation for our purposes.
    format!("{s}")
}

fn append_resp_argv(out: &mut Vec<u8>, parts: &[&[u8]]) {
    out.extend_from_slice(format!("*{}\r\n", parts.len()).as_bytes());
    for p in parts {
        out.extend_from_slice(format!("${}\r\n", p.len()).as_bytes());
        out.extend_from_slice(p);
        out.extend_from_slice(b"\r\n");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kevy_resp::Argv;

    fn argv(parts: &[&[u8]]) -> Argv {
        let mut a = Argv::default();
        for p in parts {
            a.push(p);
        }
        a
    }

    fn fresh_store() -> Store {
        Store::new()
    }

    #[test]
    fn serialize_prefix_emits_set_for_strings() {
        let mut store = fresh_store();
        store.set(b"app:foo", b"v1".to_vec(), None, false, false);
        store.set(b"app:bar", b"v2".to_vec(), None, false, false);
        store.set(b"other:k", b"v3".to_vec(), None, false, false);

        let (bulk, count) = serialize_prefix(&mut store, b"app:");
        assert_eq!(count, 2, "two string keys under prefix");
        let s = String::from_utf8_lossy(&bulk);
        assert!(s.contains("$3\r\nSET\r\n"), "wire shape has SET: {s:?}");
        assert!(s.contains("app:foo"), "key 1 present");
        assert!(s.contains("app:bar"), "key 2 present");
        assert!(!s.contains("other:k"), "non-matching key absent");
    }

    #[test]
    fn serialize_prefix_emits_hset_for_hash_in_order() {
        let mut store = fresh_store();
        store
            .hset(b"app:h", &[(b"f1".to_vec(), b"v1".to_vec()), (b"f2".to_vec(), b"v2".to_vec())])
            .unwrap();
        let (bulk, count) = serialize_prefix(&mut store, b"app:");
        assert_eq!(count, 1);
        let s = String::from_utf8_lossy(&bulk);
        assert!(s.contains("HSET"), "HSET emitted: {s:?}");
    }

    #[test]
    fn serialize_prefix_skips_non_matching_keys() {
        let mut store = fresh_store();
        store.set(b"foo", b"v".to_vec(), None, false, false);
        let (bulk, count) = serialize_prefix(&mut store, b"app:");
        assert_eq!(count, 0);
        assert!(bulk.is_empty());
    }

    #[test]
    fn ingest_handler_applies_embedded_commands_and_replies_ok() {
        let mut store = fresh_store();
        // Build a bulk of two embedded SET commands.
        let mut bulk = Vec::new();
        append_resp_argv(&mut bulk, &[b"SET", b"app:a", b"1"]);
        append_resp_argv(&mut bulk, &[b"SET", b"app:b", b"2"]);
        let args = argv(&[b"MOVE-SCOPE-INGEST", b"app:", &bulk]);
        let mut out = Vec::new();
        cmd_move_scope_ingest(&mut store, &args, &mut out);
        assert_eq!(out, b"+OK 2\r\n", "wire reply shape");
        // Store now carries both keys.
        assert_eq!(
            store.get(b"app:a").map(|v| v.map(|c| c.into_owned())),
            Ok(Some(b"1".to_vec()))
        );
        assert_eq!(
            store.get(b"app:b").map(|v| v.map(|c| c.into_owned())),
            Ok(Some(b"2".to_vec()))
        );
    }

    #[test]
    fn ingest_handler_rejects_wrong_arity() {
        let mut store = fresh_store();
        let args = argv(&[b"MOVE-SCOPE-INGEST", b"only-one"]);
        let mut out = Vec::new();
        cmd_move_scope_ingest(&mut store, &args, &mut out);
        assert!(out.starts_with(b"-ERR"), "got {:?}", String::from_utf8_lossy(&out));
    }

    #[test]
    fn move_scope_rejects_bad_syntax() {
        let mut store = fresh_store();
        // Missing FROM keyword.
        let args = argv(&[b"MOVE-SCOPE", b"p:", b"NOT-FROM", b"A", b"TO", b"B"]);
        let mut out = Vec::new();
        cmd_move_scope(&mut store, &args, &mut out);
        assert!(out.starts_with(b"-ERR"));
    }

    #[test]
    fn move_scope_rejects_when_self_node_id_not_configured() {
        let mut store = fresh_store();
        // scope_integration::self_node_id() returns None by default in
        // this test binary (install_self_id is never called). The
        // handler should refuse cleanly rather than panic.
        let args = argv(&[b"MOVE-SCOPE", b"p:", b"FROM", b"A", b"TO", b"B"]);
        let mut out = Vec::new();
        cmd_move_scope(&mut store, &args, &mut out);
        assert!(out.starts_with(b"-ERR"));
        let s = String::from_utf8_lossy(&out);
        assert!(s.contains("node_id is not configured") || s.contains("from-id"), "{s}");
    }
}
