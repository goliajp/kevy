//! Snapshot writer: serialize a [`SnapshotSource`] into the binary
//! snapshot format (`crate::snapshot_fmt`), either to a durable temp
//! file (the SAVE path) or to any `Write` sink (the replication ship
//! path). Split out of `lib.rs` to keep it under the 500-LOC house cap.

use crate::SnapshotSource;
use crate::snapshot_fmt::{
    OP_EOF, OP_HASH, OP_HFTTL, OP_LIST, OP_SET, OP_STR, OP_STREAM, OP_ZSET, SNAPSHOT_BUF_CAP,
    MAGIC, OP_SEGSTUB, VERSION, VERSION_FEED_CURSOR, VERSION_HASH_TTL, VERSION_SEG_STUB,
    write_bytes, write_ttl,
};
use crate::snapshot_payload;
use kevy_store::Value;
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::Path;

/// Write a point-in-time snapshot of `src` (a live [`kevy_store::Store`] or a
/// frozen [`kevy_store::SnapshotView`]) to `path`, atomically: data is written
/// to `<path>.tmp`, fsynced, then renamed over `path`.
pub fn save_snapshot<S: SnapshotSource>(src: &S, path: &Path) -> io::Result<()> {
    let tmp = write_snapshot_tmp(src, path)?;
    std::fs::rename(&tmp, path)
}

/// Serialize a point-in-time snapshot of `src` into any `Write` sink.
/// Used by both [`write_snapshot_tmp`] (sink = `BufWriter<File>` +
/// extra fsync after) and the v3-cluster replication path (sink =
/// `&mut Vec<u8>` for in-memory snapshot ship, see
/// `kevy-replicate/docs/snapshot.md`).
///
/// On-disk bytes are identical regardless of sink — the same magic +
/// version header, same entry stream, same `OP_EOF` trailer. Callers
/// that need durability (disk) wrap in `BufWriter<File>` and call
/// `sync_all` themselves; callers that need bytes (network ship)
/// pass a `Vec<u8>`.
pub fn write_snapshot_to<S: SnapshotSource, W: Write>(src: &S, sink: &mut W) -> io::Result<()> {
    write_snapshot_to_with_cursor(src, sink, None)
}

/// [`write_snapshot_to`] with the recovery-point header: when
/// `cursor = Some((generation, offset))` the snapshot records the feed
/// position it was taken at (format v5); `None` writes the legacy v4
/// stream unchanged.
pub fn write_snapshot_to_with_cursor<S: SnapshotSource, W: Write>(
    src: &S,
    sink: &mut W,
    cursor: Option<(u64, u64)>,
) -> io::Result<()> {
    // Field-TTL records force format v6; collect them first so
    // the header version is known before anything is written.
    let mut fttl: Vec<(Vec<u8>, Vec<u8>, u64)> = Vec::new();
    src.for_each_hash_ttl(|k, f, d| fttl.push((k.to_vec(), f.to_vec(), d)));
    let mut w = BufWriter::with_capacity(SNAPSHOT_BUF_CAP, sink);
    w.write_all(MAGIC)?;
    let version = snapshot_version(src, !fttl.is_empty(), cursor.is_some());
    w.write_all(&[version])?;
    if version >= VERSION_FEED_CURSOR {
        let (generation, offset) = cursor.unwrap_or((0, 0));
        w.write_all(&generation.to_le_bytes())?;
        w.write_all(&offset.to_le_bytes())?;
    }
    // The source yields *remaining* ms; v3 persists the absolute
    // Unix-ms deadline (now + remaining) so the TTL survives a restart.
    let now = kevy_store::now_unix_ms();
    // Enumeration is infallible; capture the first write error to surface.
    let mut err: Option<io::Error> = None;
    src.for_each_entry(|key, value, ttl| {
        let deadline = ttl.map(|ms| now.saturating_add(ms));
        if err.is_none()
            && let Err(e) = write_entry(&mut w, key, value, deadline)
        {
            err = Some(e);
        }
    });
    if let Some(e) = err {
        return Err(e);
    }
    for (k, f, d) in &fttl {
        w.write_all(&[OP_HFTTL])?;
        write_bytes(&mut w, k)?;
        write_bytes(&mut w, f)?;
        w.write_all(&d.to_le_bytes())?;
    }
    w.write_all(&[OP_EOF])?;
    w.flush()?;
    Ok(())
}

/// The highest format feature the stream needs — lower versions stay
/// byte-identical for stores that never used the newer features.
fn snapshot_version<S: SnapshotSource>(src: &S, has_fttl: bool, has_cursor: bool) -> u8 {
    if !src.row_seg_files().is_empty() {
        VERSION_SEG_STUB
    } else if has_fttl {
        VERSION_HASH_TTL
    } else if has_cursor {
        VERSION_FEED_CURSOR
    } else {
        VERSION
    }
}

/// The write half of [`save_snapshot`]: produce the durable (fsynced)
/// `<path>.tmp` and return its path **without** the final rename. For the
/// COW background-save flow: the serializer thread writes the temp file at
/// leisure, then the store-owning thread renames it in the same critical
/// section that resets the AOF — keeping the snapshot/AOF commit adjacent
/// instead of seconds apart.
pub fn write_snapshot_tmp<S: SnapshotSource>(src: &S, path: &Path) -> io::Result<std::path::PathBuf> {
    let tmp = tmp_path(path);
    {
        let mut file = File::create(&tmp)?;
        write_snapshot_to(src, &mut file)?;
        file.sync_all()?; // durably on disk before the rename
    }
    Ok(tmp)
}

/// Serialize one entry: `[op][ttl][key][payload]`.
fn write_entry<W: Write>(w: &mut W, key: &[u8], value: &Value, ttl: Option<u64>) -> io::Result<()> {
    let op = op_for(value)?;
    w.write_all(&[op])?;
    write_ttl(w, ttl)?;
    write_bytes(w, key)?;
    write_payload(w, value)
}

/// The wire opcode for a value (encoding-agnostic per type).
fn op_for(value: &Value) -> io::Result<u8> {
    Ok(match value {
        Value::Str(_) | Value::Int(_) | Value::ArcBulk(_) => OP_STR, // L1/L2: all reuse OP_STR.

        Value::Hash(_) | Value::SegHash(_) | Value::SmallHashInline(_) => OP_HASH,
        Value::List(_) | Value::SegList(_) | Value::SmallListInline(_) => OP_LIST,
        // A.7 O5: both Set encodings share the OP_SET wire format —
        // payload is `[len: u32 LE][bulk: len-prefixed bytes]*`, agnostic
        // of whether the in-memory representation is `SmallSetInline` or
        // `Arc<KevySet>`.
        Value::Set(_) | Value::SegSet(_) | Value::SmallSetInline(_) => OP_SET,
        Value::ZSet(_) | Value::SmallZSetInline(_) => OP_ZSET,
        Value::Stream(_) => OP_STREAM,
        // Seg-backed stub: persist the REFERENCE — the row's data is
        // truth in the segment directory beside this snapshot.
        Value::Cold(c) if c.seg_parts().is_some() => OP_SEGSTUB,
        // Every SnapshotSource materializes VLOG stubs before yielding
        // them — one here means a producer bypassed the contract. Fail
        // the snapshot rather than silently dropping the value.
        Value::Cold(_) => {
            return Err(io::Error::other(
                "vlog stub reached the snapshot writer — SnapshotSource must materialize (T4)",
            ));
        }
    })
}

/// The per-encoding payload writer half of [`write_entry`].
fn write_payload<W: Write>(w: &mut W, value: &Value) -> io::Result<()> {
    match value {
        Value::Str(v) => write_bytes(w, v.as_slice()),
        Value::Int(n) => write_bytes(w, n.to_string().as_bytes()),
        Value::ArcBulk(a) => write_bytes(w, a.as_ref()),
        Value::Hash(h) => snapshot_payload::write_hash_payload(w, h),
        Value::SegHash(h) => snapshot_payload::write_seghash_payload(w, h),
        Value::SmallHashInline(h) => snapshot_payload::write_small_hash_payload(w, h),
        Value::List(l) => snapshot_payload::write_list_payload(w, l),
        Value::SegList(l) => snapshot_payload::write_seglist_payload(w, l),
        Value::SmallListInline(l) => snapshot_payload::write_small_list_payload(w, l),
        Value::Set(set) => snapshot_payload::write_set_payload(w, set),
        Value::SegSet(set) => snapshot_payload::write_segset_payload(w, set),
        Value::SmallSetInline(s) => snapshot_payload::write_small_set_payload(w, s),
        Value::ZSet(z) => snapshot_payload::write_zset_payload(w, z),
        Value::SmallZSetInline(z) => snapshot_payload::write_small_zset_payload(w, z),
        Value::Stream(s) => snapshot_payload::write_stream_payload(w, s),
        Value::Cold(c) => {
            let (seq, weight) = c.seg_parts().expect("vlog stubs error in the op match");
            w.write_all(&seq.to_le_bytes())?;
            w.write_all(&weight.to_le_bytes())
        }
    }
}

/// v4 consumer-group section: `[n_groups][per group: name, last_delivered,
/// consumers (name + last_seen_ms), PEL rows]`. Tombstone PEL rows are kept
/// — the snapshot path is the full-fidelity one (the AOF rewrite can't
/// re-create them via XCLAIM, see `rewrite_fmt`).
pub(crate) fn write_stream_groups<W: Write>(w: &mut W, groups: &[kevy_store::LoadedGroup]) -> io::Result<()> {
    w.write_all(&(groups.len() as u32).to_le_bytes())?;
    for g in groups {
        write_bytes(w, &g.name)?;
        w.write_all(&g.last_delivered.0.to_le_bytes())?;
        w.write_all(&g.last_delivered.1.to_le_bytes())?;
        w.write_all(&(g.consumers.len() as u32).to_le_bytes())?;
        for (name, last_seen_ms) in &g.consumers {
            write_bytes(w, name)?;
            w.write_all(&last_seen_ms.to_le_bytes())?;
        }
        w.write_all(&(g.pel.len() as u32).to_le_bytes())?;
        for (ms, seq, consumer, delivery_time_ms, delivery_count) in &g.pel {
            w.write_all(&ms.to_le_bytes())?;
            w.write_all(&seq.to_le_bytes())?;
            write_bytes(w, consumer)?;
            w.write_all(&delivery_time_ms.to_le_bytes())?;
            w.write_all(&delivery_count.to_le_bytes())?;
        }
    }
    Ok(())
}

fn tmp_path(path: &Path) -> std::path::PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".tmp");
    s.into()
}
