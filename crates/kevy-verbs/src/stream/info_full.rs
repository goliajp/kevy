//! `XINFO STREAM key FULL [COUNT n]`: the stream head, its first `n`
//! entries, and every group with its first `n` pending entries and every
//! consumer with its own first `n`. `n` of 0 means no limit.

use kevy_resp::{RespVersion, encode_array_len, encode_bulk, encode_integer};
use kevy_store::{ConsumerGroup, PelEntry, StreamData, StreamId};

use super::{
    emit_entry, emit_read_and_lag, emit_stream_head, encode_id, field, map_header,
    sorted_consumers, sorted_groups,
};

pub(super) fn emit(out: &mut Vec<u8>, s: &StreamData, count: usize, proto: RespVersion) {
    let limit = if count == 0 { usize::MAX } else { count };
    map_header(out, proto, 9);
    emit_stream_head(out, s);
    field(out, "entries");
    let shown = s.length().min(limit as u64);
    encode_array_len(out, shown as i64);
    for (id, fv) in s.entries().take(limit) {
        emit_entry(out, id, fv);
    }
    field(out, "groups");
    let groups = sorted_groups(s);
    encode_array_len(out, groups.len() as i64);
    for (name, g) in groups {
        emit_group(out, s, name, g, limit, proto);
    }
}

fn emit_group(
    out: &mut Vec<u8>,
    s: &StreamData,
    name: &[u8],
    g: &ConsumerGroup,
    limit: usize,
    proto: RespVersion,
) {
    map_header(out, proto, 7);
    field(out, "name");
    encode_bulk(out, name);
    field(out, "last-delivered-id");
    encode_id(out, g.last_delivered_id());
    emit_read_and_lag(out, s, g, proto);
    field(out, "pel-count");
    encode_integer(out, g.pending_count() as i64);
    field(out, "pending");
    encode_array_len(out, g.pending_count().min(limit) as i64);
    for (id, p) in g.pending_range(..).take(limit) {
        encode_array_len(out, 4);
        encode_id(out, id);
        encode_bulk(out, p.consumer.as_slice());
        encode_integer(out, p.delivery_time_ms as i64);
        encode_integer(out, i64::try_from(p.delivery_count).unwrap_or(i64::MAX));
    }
    field(out, "consumers");
    emit_consumers(out, g, limit, proto);
}

/// Every consumer by name, each with the first `limit` pending entries
/// it holds, gathered in one walk of the pending list.
fn emit_consumers(out: &mut Vec<u8>, g: &ConsumerGroup, limit: usize, proto: RespVersion) {
    let consumers = sorted_consumers(g);
    let mut held: Vec<Vec<(StreamId, &PelEntry)>> = vec![Vec::new(); consumers.len()];
    for (id, p) in g.pending_range(..) {
        let owner = consumers.binary_search_by_key(&p.consumer.as_slice(), |(name, _)| *name);
        if let Ok(i) = owner
            && held[i].len() < limit
        {
            held[i].push((id, p));
        }
    }
    encode_array_len(out, consumers.len() as i64);
    for ((name, c), rows) in consumers.iter().zip(&held) {
        map_header(out, proto, 5);
        field(out, "name");
        encode_bulk(out, name);
        field(out, "seen-time");
        encode_integer(out, c.last_seen_ms() as i64);
        field(out, "active-time");
        encode_integer(out, c.last_active_ms().map_or(-1, |at| at as i64));
        field(out, "pel-count");
        encode_integer(out, c.pending_count() as i64);
        field(out, "pending");
        encode_array_len(out, rows.len() as i64);
        for (id, p) in rows {
            encode_array_len(out, 3);
            encode_id(out, *id);
            encode_integer(out, p.delivery_time_ms as i64);
            encode_integer(out, i64::try_from(p.delivery_count).unwrap_or(i64::MAX));
        }
    }
}
