//! Prefix serialization for MOVE-SCOPE (the exporter half): walk the
//! source shard, reconstruct every matching key as replayable RESP
//! frames. Split from `scope_move.rs` for the 500-LOC house rule.

use kevy_store::Store;

/// Walk `store`, collect every key matching `prefix`, reconstruct
/// each as one (or two — for TTL'd keys) RESP frame. Returns the
/// concatenated wire bytes + the frame count.
///
/// T6: the whole walk runs inside the bulk-read peek scope — a cold
/// row serializes from ONE record read without promoting or advancing
/// the 2nd-touch gate (a scope export must not thrash the hot tier).
pub(super) fn serialize_prefix(store: &mut Store, prefix: &[u8]) -> (Vec<u8>, usize) {
    store.peek_scope(|s| serialize_prefix_rows(s, prefix))
}

fn serialize_prefix_rows(store: &mut Store, prefix: &[u8]) -> (Vec<u8>, usize) {
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
            "stream" => super::scope_move_stream::emit_stream(store, &key, &mut bulk, &mut count),
            _ => continue, // none — key raced away between collect and here
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

pub(super) fn append_resp_argv(out: &mut Vec<u8>, parts: &[&[u8]]) {
    out.extend_from_slice(format!("*{}\r\n", parts.len()).as_bytes());
    for p in parts {
        out.extend_from_slice(format!("${}\r\n", p.len()).as_bytes());
        out.extend_from_slice(p);
        out.extend_from_slice(b"\r\n");
    }
}
