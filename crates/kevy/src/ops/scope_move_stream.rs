//! Stream leg of the scope-migration serializer. A stream key is
//! reconstructed as RESP command frames (`XADD` per entry, `XSETID`
//! for scalar state, `XGROUP CREATE`/`CREATECONSUMER` and
//! `XCLAIM … TIME … RETRYCOUNT … FORCE JUSTID` for consumer groups
//! and their PEL rows) via the shared emitter in `kevy_persist`, so
//! the ingest side replays them through the normal dispatch surface.
//!
//! Fidelity contract mirrors the AOF rewrite: entries, `last_id` /
//! `entries_added` / `max_deleted_id`, groups, consumers, and live
//! PEL rows (owner + delivery time + delivery count) all survive the
//! move. Tombstone PEL rows (entries deleted while still pending)
//! are skipped — no RESP verb can recreate those.

use kevy_store::Store;

/// Append the rebuild frames for the stream at `key` to `bulk`,
/// bumping `count` by the number of frames emitted.
pub(super) fn emit_stream(store: &mut Store, key: &[u8], bulk: &mut Vec<u8>, count: &mut usize) {
    let Ok(Some(view)) = store.stream_view(key) else { return };
    if let Ok(frames) = kevy_persist::write_stream_as_commands(bulk, key, view) {
        *count += frames;
    }
}

#[cfg(test)]
mod tests {
    use kevy_resp::Argv;
    use kevy_store::{GroupCreateMode, ReadGroupId, Store, StreamId, XAddIdSpec};

    use super::super::scope_move::serialize_prefix;

    fn argv(parts: &[&[u8]]) -> Argv {
        let mut a = Argv::default();
        for p in parts {
            a.push(p);
        }
        a
    }

    fn id(ms: u64, seq: u64) -> XAddIdSpec {
        XAddIdSpec::Explicit(StreamId { ms, seq })
    }

    /// Source stream: three entries (one later deleted), a consumer
    /// group with one read-but-unacked entry (live PEL row), plus a
    /// second consumer known to the group but with an empty PEL.
    fn seed_stream(store: &mut Store, key: &[u8]) {
        for (ms, f, v) in [(1u64, "f1", "v1"), (2, "f2", "v2"), (3, "f3", "v3")] {
            store.xadd(key, id(ms, 0), vec![(f.into(), v.into())], false, 0).unwrap();
        }
        store
            .xgroup_create(key, b"g1", GroupCreateMode::AtId(StreamId { ms: 1, seq: 0 }), false)
            .unwrap();
        store.xreadgroup(key, b"g1", b"alice", ReadGroupId::New, Some(1), false, 777).unwrap();
        store.xgroup_create_consumer(key, b"g1", b"bob", 778).unwrap();
        store.xdel(key, &[StreamId { ms: 3, seq: 0 }]).unwrap();
    }

    fn ingest_into(dst: &mut Store, prefix: &[u8], bulk: &[u8]) {
        let args = argv(&[b"MOVE-SCOPE-INGEST", prefix, bulk]);
        let mut out = Vec::new();
        let c = crate::KevyCommands::new();
        super::super::scope_move::cmd_move_scope_ingest(&c.ctx(), dst, &args, &mut out);
        assert!(out.starts_with(b"+OK"), "ingest failed: {:?}", String::from_utf8_lossy(&out));
    }

    #[test]
    fn stream_roundtrips_entries_and_scalar_state() {
        let mut src = Store::new();
        seed_stream(&mut src, b"app:x");
        let (bulk, count) = serialize_prefix(&mut src, b"app:");
        assert!(count > 0, "stream key must emit frames");

        let mut dst = Store::new();
        ingest_into(&mut dst, b"app:", &bulk);

        let sv = src.stream_view(b"app:x").unwrap().unwrap();
        let (s_entries, s_last, s_added, s_mxd) = (
            sv.iter_entries().map(|(i, fv)| (i, fv.to_vec())).collect::<Vec<_>>(),
            sv.last_id(),
            sv.entries_added(),
            sv.max_deleted_id(),
        );
        let dv = dst.stream_view(b"app:x").unwrap().unwrap();
        let d_entries = dv.iter_entries().map(|(i, fv)| (i, fv.to_vec())).collect::<Vec<_>>();
        assert_eq!(d_entries, s_entries, "entries survive the move");
        assert_eq!(dv.last_id(), s_last, "last_id survives");
        assert_eq!(dv.entries_added(), s_added, "entries_added survives");
        assert_eq!(dv.max_deleted_id(), s_mxd, "max_deleted_id survives");
    }

    #[test]
    fn stream_roundtrips_groups_consumers_and_pel() {
        let mut src = Store::new();
        seed_stream(&mut src, b"app:x");
        let (bulk, _) = serialize_prefix(&mut src, b"app:");
        let mut dst = Store::new();
        ingest_into(&mut dst, b"app:", &bulk);

        let sg = &src.stream_view(b"app:x").unwrap().unwrap().export_groups()[0];
        let dg = &dst.stream_view(b"app:x").unwrap().unwrap().export_groups()[0];
        assert_eq!(dg.name, sg.name);
        assert_eq!(dg.last_delivered, sg.last_delivered, "last_delivered_id survives");
        let mut s_names: Vec<_> = sg.consumers.iter().map(|(n, _)| n.clone()).collect();
        let mut d_names: Vec<_> = dg.consumers.iter().map(|(n, _)| n.clone()).collect();
        s_names.sort();
        d_names.sort();
        assert_eq!(d_names, s_names, "both consumers survive, PEL or not");
        assert_eq!(dg.pel, sg.pel, "PEL rows survive with owner/time/count");
    }

    #[test]
    fn empty_stream_with_advanced_clock_roundtrips() {
        let mut src = Store::new();
        src.xadd(b"app:e", id(9, 1), vec![(b"f".to_vec(), b"v".to_vec())], false, 0).unwrap();
        src.xdel(b"app:e", &[StreamId { ms: 9, seq: 1 }]).unwrap();
        let (bulk, _) = serialize_prefix(&mut src, b"app:");
        let mut dst = Store::new();
        ingest_into(&mut dst, b"app:", &bulk);
        let dv = dst.stream_view(b"app:e").unwrap().unwrap();
        assert_eq!(dv.length(), 0);
        assert_eq!(dv.last_id(), StreamId { ms: 9, seq: 1 }, "ID clock survives");
    }
}
