//! `MOVE-SCOPE` + `MOVE-SCOPE-INGEST` — scope migration operator
//! commands.
//!
//! Quiesce-window mechanism.
//! Operator runs `MOVE-SCOPE <prefix> FROM <from-id> TO <to-id>`
//! against the source writer. The writer:
//!
//! 1. Validates: self is `<from-id>`, `<to-id>` resolves to a
//!    `host:port` in the peer table.
//! 2. Flips the local migration state to MIGRATING; subsequent
//!    writes for the prefix return `-QUIESCED migrating to
//!    <to-host:port>` (wired through scope routing in dispatch).
//! 3. Serializes the prefix's keyspace slice (all 6 data types:
//!    string / hash / list / set / zset / stream — streams carry
//!    entries, scalar state, consumer groups, and live PEL rows;
//!    TTLs as absolute `PEXPIREAT`).
//! 4. Connects to the target's data port and sends one
//!    `MOVE-SCOPE-INGEST <prefix> <bulk>` command.
//! 5. On `+OK`, commits the migration locally; future writes for
//!    the prefix on the source return `-MISDIRECTED writer is
//!    <to-host:port>` (no quiesce — move done).
//! 6. On error, aborts the migration; writes for the prefix on
//!    the source resume.
//!
//! Target side handler bypasses scope routing during the ingest
//! window via the shard's ingest guard, then dispatches each embedded
//! command normally through `crate::dispatch::dispatch_into`.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use kevy_resp::{ArgvView, encode_error};
use kevy_store::Store;

use crate::state::Ctx;
// Serialization half lives in `scope_move_emit` (500-LOC split);
// re-exported so `scope_move::serialize_prefix` callers keep working.
pub(super) use super::scope_move_emit::serialize_prefix;
pub(crate) use super::scope_move_ingest::cmd_move_scope_ingest;

/// `MOVE-SCOPE <prefix> FROM <from-id> TO <to-id>` — operator-issued
/// scope migration.
pub(crate) fn cmd_move_scope<A: ArgvView + ?Sized>(
    ctx: &Ctx<'_>,
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
) {
    let Some((prefix_owned, from_id, to_id)) = parse_move_scope_args(args, out) else {
        return;
    };
    let Some(target_addr) = validate_move_scope_route(ctx, &from_id, &to_id, out) else {
        return;
    };

    // Start the migration locally. From this instant, dispatch
    // routes writes for this prefix to `-QUIESCED migrating to
    // <to_addr>`. Each migration transition is a cold gate
    // writer: bump the control epoch after the table change.
    if let Err(e) = ctx.state.scope.migration_start(
        prefix_owned.clone(),
        from_id.to_string(),
        to_id.to_string(),
    ) {
        return encode_error(out, &format!("ERR MOVE-SCOPE: {e}"));
    }
    ctx.state.bump_control_epoch();

    // Ship.
    match ship_prefix_to_target(store, &prefix_owned, &target_addr) {
        Ok(count) => {
            ctx.state.scope.migration_commit(&prefix_owned);
            ctx.state.bump_control_epoch();
            let reply = format!("+OK {count}\r\n");
            out.extend_from_slice(reply.as_bytes());
        }
        Err(e) => {
            ctx.state.scope.migration_abort(&prefix_owned);
            ctx.state.bump_control_epoch();
            encode_error(out, &format!("ERR MOVE-SCOPE ship failed: {e}"));
        }
    }
}

/// Parse `MOVE-SCOPE <prefix> FROM <from-id> TO <to-id>`: arity,
/// keyword, and UTF-8 checks. On failure the error reply has been
/// written to `out` and `None` is returned.
fn parse_move_scope_args<A: ArgvView + ?Sized>(
    args: &A,
    out: &mut Vec<u8>,
) -> Option<(Vec<u8>, String, String)> {
    if args.len() != 6 {
        encode_error(
            out,
            "ERR wrong number of arguments — MOVE-SCOPE <prefix> FROM <from-id> TO <to-id>",
        );
        return None;
    }
    let Some(prefix) = args.get(1) else {
        wrong_syntax(out);
        return None;
    };
    let from_kw = args.get(2).unwrap_or_default();
    let from_id = args.get(3).unwrap_or_default();
    let to_kw = args.get(4).unwrap_or_default();
    let to_id = args.get(5).unwrap_or_default();
    if !from_kw.eq_ignore_ascii_case(b"FROM") || !to_kw.eq_ignore_ascii_case(b"TO") {
        wrong_syntax(out);
        return None;
    }
    let Ok(from_id) = std::str::from_utf8(from_id) else {
        wrong_syntax(out);
        return None;
    };
    let Ok(to_id) = std::str::from_utf8(to_id) else {
        wrong_syntax(out);
        return None;
    };
    Some((prefix.to_vec(), from_id.to_string(), to_id.to_string()))
}

/// Validate the migration route: self must be `<from-id>` and
/// `<to-id>` must resolve in the peer table. Returns the target's
/// `host:port`; on failure the error reply has been written to `out`.
fn validate_move_scope_route(
    ctx: &Ctx<'_>,
    from_id: &str,
    to_id: &str,
    out: &mut Vec<u8>,
) -> Option<String> {
    // Self must be the source. Local writes flow only through this
    // node's keyspace; a misdirected MOVE-SCOPE would silently lose
    // half the data.
    match ctx.state.scope.self_node_id() {
        Some(me) if me == from_id => {}
        Some(me) => {
            encode_error(
                out,
                &format!("ERR MOVE-SCOPE: from-id {from_id:?} is not this node ({me:?})"),
            );
            return None;
        }
        None => {
            encode_error(
                out,
                "ERR MOVE-SCOPE: [cluster] node_id is not configured on this node",
            );
            return None;
        }
    }
    let Some(target_addr) = ctx.state.scope.peer_addr(to_id) else {
        encode_error(
            out,
            &format!("ERR MOVE-SCOPE: target node {to_id:?} not in [cluster] peers"),
        );
        return None;
    };
    // The dispatch write-quiesce gate only consults the migration
    // table when an ownership table exists (`is_active`); without
    // declared scopes a move ships while local writes keep landing.
    if !ctx.state.scope.is_active() {
        encode_error(
            out,
            "ERR MOVE-SCOPE: no [cluster] scopes declared on this node — \
             writes would not quiesce during the move",
        );
        return None;
    }
    Some(target_addr)
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

#[cfg(test)]
mod tests {
    use super::super::scope_move_emit::append_resp_argv;
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
            .hset(b"app:h", &[(b"f1".as_slice(), b"v1".as_slice()), (b"f2".as_slice(), b"v2".as_slice())])
            .unwrap();
        let (bulk, count) = serialize_prefix(&mut store, b"app:");
        assert_eq!(count, 1);
        let s = String::from_utf8_lossy(&bulk);
        assert!(s.contains("HSET"), "HSET emitted: {s:?}");
    }

    /// MOVE-SCOPE is how a prefix changes owner, so what it can carry
    /// bounds what a reshard can move. This pins the whole set —
    /// **including streams and TTLs**, both of which the client-side
    /// rebuild path in `kevy-cli` does not carry (streams) or carries
    /// differently (absolute PEXPIREAT vs PEXPIREAT here too).
    ///
    /// A type added to the engine without an arm here would reshard
    /// into nothing, and the emitter's catch-all is a bare `continue`
    /// — the same silence that lost streams from `kevy-cli export`.
    #[test]
    fn serialize_prefix_carries_every_type_and_the_ttl() {
        let mut store = fresh_store();
        store.set(b"app:s", b"v".to_vec(), None, false, false);
        store.hset(b"app:h", &[(b"f".as_slice(), b"v".as_slice())]).unwrap();
        store.rpush(b"app:l", &[b"a".as_slice()]).unwrap();
        store.sadd(b"app:set", &[b"m".as_slice()]).unwrap();
        store.zadd(b"app:z", &[(1.0, b"m".as_slice())]).unwrap();
        store
            .xadd(
                b"app:st",
                kevy_store::XAddIdSpec::AutoAll,
                vec![(b"f".to_vec(), b"v".to_vec())],
                false,
                1,
            )
            .unwrap();
        store.set(
            b"app:ttl",
            b"v".to_vec(),
            Some(std::time::Duration::from_secs(60)),
            false,
            false,
        );

        let (bulk, _count) = serialize_prefix(&mut store, b"app:");
        let w = String::from_utf8_lossy(&bulk);
        for (key, verb) in [
            ("app:s", "SET"),
            ("app:h", "HSET"),
            ("app:l", "RPUSH"),
            ("app:set", "SADD"),
            ("app:z", "ZADD"),
            ("app:st", "XADD"),
        ] {
            assert!(w.contains(key), "{key} must ship: {w:?}");
            assert!(w.contains(verb), "{key} needs its rebuild verb {verb}: {w:?}");
        }
        assert!(w.contains("PEXPIREAT"), "a TTL'd key ships its deadline: {w:?}");
    }

    /// A frame the target REFUSES must fail the whole ingest, by name.
    ///
    /// It used to reply `+OK 1`: the dispatch reply was written into a
    /// scratch buffer and never read, so a WRONGTYPE counted as
    /// applied. The source commits on that count — ownership of the
    /// prefix moves to a node that does not have the row, and the row
    /// is then unreachable from either side.
    #[test]
    fn a_refused_frame_fails_the_ingest_and_names_the_key() {
        let kevy = crate::KevyCommands::with_state(std::sync::Arc::new(
            crate::RuntimeState::new(
                std::sync::Arc::new(kevy_config::Config::default()),
                std::path::PathBuf::new(),
                1,
            )
            .unwrap(),
        ));
        let mut store = fresh_store();
        // The target already holds this key as a STRING; the shipped
        // frame rebuilds it as a LIST, and RPUSH on a string is refused.
        store.set(b"app:x", b"already-a-string".to_vec(), None, false, false);
        let mut bulk = Vec::new();
        append_resp_argv(&mut bulk, &[b"RPUSH", b"app:x", b"a"]);

        let args = argv(&[b"MOVE-SCOPE-INGEST", b"app:", &bulk]);
        let reply = String::from_utf8_lossy(&kevy.dispatch(&mut store, &args)).into_owned();

        assert!(reply.starts_with('-'), "a refused frame must not answer +OK: {reply:?}");
        assert!(reply.contains("app:x"), "the reply must name the key: {reply:?}");
        assert!(reply.contains("WRONGTYPE"), "and why it was refused: {reply:?}");
    }

    /// The ordinary path still answers with a count.
    #[test]
    fn an_accepted_ingest_still_reports_how_many_applied() {
        let kevy = crate::KevyCommands::with_state(std::sync::Arc::new(
            crate::RuntimeState::new(
                std::sync::Arc::new(kevy_config::Config::default()),
                std::path::PathBuf::new(),
                1,
            )
            .unwrap(),
        ));
        let mut store = fresh_store();
        let mut bulk = Vec::new();
        append_resp_argv(&mut bulk, &[b"SET", b"app:a", b"1"]);
        append_resp_argv(&mut bulk, &[b"SET", b"app:b", b"2"]);
        let args = argv(&[b"MOVE-SCOPE-INGEST", b"app:", &bulk]);
        let reply = String::from_utf8_lossy(&kevy.dispatch(&mut store, &args)).into_owned();
        assert!(reply.starts_with("+OK 2"), "two frames applied: {reply:?}");
        assert_eq!(store.type_of(b"app:a"), "string");
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
        let c = crate::KevyCommands::new();
        cmd_move_scope_ingest(&c.ctx(), &mut store, &args, &mut out);
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
        let c = crate::KevyCommands::new();
        cmd_move_scope_ingest(&c.ctx(), &mut store, &args, &mut out);
        assert!(out.starts_with(b"-ERR"), "got {:?}", String::from_utf8_lossy(&out));
    }

    #[test]
    fn move_scope_rejects_bad_syntax() {
        let mut store = fresh_store();
        // Missing FROM keyword.
        let args = argv(&[b"MOVE-SCOPE", b"p:", b"NOT-FROM", b"A", b"TO", b"B"]);
        let mut out = Vec::new();
        let c = crate::KevyCommands::new();
        cmd_move_scope(&c.ctx(), &mut store, &args, &mut out);
        assert!(out.starts_with(b"-ERR"));
    }

    #[test]
    fn move_scope_rejects_when_self_node_id_not_configured() {
        let mut store = fresh_store();
        // A default state has no `[cluster] node_id`, so
        // `scope.self_node_id()` is `None`. The handler should refuse
        // cleanly rather than panic.
        let args = argv(&[b"MOVE-SCOPE", b"p:", b"FROM", b"A", b"TO", b"B"]);
        let mut out = Vec::new();
        let c = crate::KevyCommands::new();
        cmd_move_scope(&c.ctx(), &mut store, &args, &mut out);
        assert!(out.starts_with(b"-ERR"));
        let s = String::from_utf8_lossy(&out);
        assert!(s.contains("node_id is not configured") || s.contains("from-id"), "{s}");
    }
}
