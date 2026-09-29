//! Prefix serialization for MOVE-SCOPE (the exporter half): walk the
//! source shard, reconstruct every matching key as replayable RESP
//! frames. Split from `scope_move.rs` for the 500-LOC house rule.

use kevy_store::Store;

/// Walk `store`, collect every key matching `prefix`, reconstruct
/// each as one (or two — for TTL'd keys) RESP frame. Returns the
/// concatenated wire bytes + the frame count.
///
/// The whole walk runs inside the bulk-read peek scope — a cold
/// row serializes from ONE record read without promoting or advancing
/// the 2nd-touch gate (a scope export must not thrash the hot tier).
pub(super) fn serialize_prefix(store: &mut Store, prefix: &[u8]) -> (Vec<u8>, usize) {
    store.peek_scope(|s| serialize_prefix_rows(s, prefix))
}

fn serialize_prefix_rows(store: &mut Store, prefix: &[u8]) -> (Vec<u8>, usize) {
    let mut bulk = Vec::new();
    let mut count = 0usize;
    // the prefix's keys a batch at a time, not a copy of every key in the
    // shard; serializing reads rows and inserts nothing, so none is missed
    let mut walk = crate::key_walk::KeyWalk::new(prefix);
    while !walk.is_done() {
        for key in walk.next_batch(store, 1024) {
            emit_key(store, &key, &mut bulk, &mut count);
        }
    }
    (bulk, count)
}

/// One key as its frames (and a `PEXPIREAT` when it has a deadline).
fn emit_key(store: &mut Store, key: &[u8], bulk: &mut Vec<u8>, count: &mut usize) {
    let ttl_ms = store.pttl(key);
    let abs_expire = (ttl_ms > 0).then(|| kevy_store::now_unix_ms().saturating_add(ttl_ms as u64));
    match store.type_of(key) {
        "string" => emit_string(store, key, bulk, count),
        "hash" => emit_hash(store, key, bulk, count),
        "list" => emit_list(store, key, bulk, count),
        "set" => emit_set(store, key, bulk, count),
        "zset" => emit_zset(store, key, bulk, count),
        "stream" => super::scope_move_stream::emit_stream(store, key, bulk, count),
        _ => return,
    }
    if let Some(ms) = abs_expire {
        let ms_str = ms.to_string();
        append_resp_argv(bulk, &[b"PEXPIREAT", key, ms_str.as_bytes()]);
        *count += 1;
    }
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

pub(super) fn append_resp_argv(out: &mut Vec<u8>, parts: &[&[u8]]) {
    out.extend_from_slice(format!("*{}\r\n", parts.len()).as_bytes());
    for p in parts {
        out.extend_from_slice(format!("${}\r\n", p.len()).as_bytes());
        out.extend_from_slice(p);
        out.extend_from_slice(b"\r\n");
    }
}
