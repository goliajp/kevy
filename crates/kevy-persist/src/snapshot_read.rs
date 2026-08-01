//! Snapshot loader: parse the binary snapshot format
//! (`crate::snapshot_fmt`) back into a [`Store`], from disk or from any
//! `Read` sink (the replication apply path). Split out of `lib.rs` to
//! keep it under the 500-LOC house cap.

use crate::snapshot_fmt::{
    MAGIC, OP_EOF, OP_HASH, OP_HFTTL, OP_LIST, OP_SEGSTUB, OP_SET, OP_STR, OP_STREAM, OP_ZSET, VERSION_SEG_STUB,
    VERSION, VERSION_ABSOLUTE_TTL, VERSION_FEED_CURSOR, VERSION_RELATIVE_TTL,
    capped_capacity, read_bytes, read_ttl, read_u8, read_u32, read_u64,
};
use kevy_store::Store;
use std::fs::File;
use std::io::{self, BufReader, Read};
use std::path::Path;

/// Read the recovery-point cursor from a snapshot's header:
/// `Some((generation, offset))` for format v5+, `None` for older
/// (cursor-less) snapshots. Does not load entries.
pub fn read_snapshot_cursor(path: &Path) -> io::Result<Option<(u64, u64)>> {
    let mut r = BufReader::new(File::open(path)?);
    let mut magic = [0u8; 8];
    r.read_exact(&mut magic)?;
    if &magic != MAGIC {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "kevy snapshot: bad magic"));
    }
    let version = read_u8(&mut r)?;
    if version < VERSION_FEED_CURSOR {
        return Ok(None);
    }
    let mut gen_bytes = [0u8; 8];
    let mut off_bytes = [0u8; 8];
    r.read_exact(&mut gen_bytes)?;
    r.read_exact(&mut off_bytes)?;
    let generation = u64::from_le_bytes(gen_bytes);
    let offset = u64::from_le_bytes(off_bytes);
    Ok(Some((generation, offset)))
}

/// Load a snapshot from `path` into `store` (entries are inserted, not cleared
/// first — call on a fresh store). Errors on a bad magic/version or truncation.
pub fn load_snapshot(store: &mut Store, path: &Path) -> io::Result<()> {
    let r = BufReader::new(File::open(path)?);
    load_snapshot_from(store, r)
}

/// Load a snapshot from any [`std::io::Read`] sink into `store` —
/// symmetric to [`crate::write_snapshot_to`]. Used by the v3-cluster
/// replication path (sink = `&[u8]` wrapped in `std::io::Cursor`) to
/// apply a primary-shipped snapshot to a fresh local store without
/// touching disk. Entries are inserted, not cleared first — call on
/// a fresh store. Errors on bad magic/version or truncation.
pub fn load_snapshot_from<R: Read>(store: &mut Store, r: R) -> io::Result<()> {
    load_snapshot_filtered(store, r, |_| true)
}

/// [`load_snapshot_from`] with a key predicate: only records
/// whose key satisfies `keep` are loaded (skipped records are still
/// parsed to stay in frame). The single-source replica path broadcasts
/// one snapshot payload to every shard and each loads its own hash
/// slice — no intermediate store, no re-serialization.
pub fn load_snapshot_filtered<R: Read>(
    store: &mut Store,
    mut r: R,
    keep: impl Fn(&[u8]) -> bool,
) -> io::Result<()> {
    let version = read_snapshot_header(&mut r)?;
    // v3+ stores absolute Unix-ms deadlines; convert each to remaining ms
    // against one `now` read so the load is internally consistent. A deadline
    // already past becomes `Some(0)` → loaded then immediately reaped (lazy
    // get / active reaper), matching "expired key is gone". v2 ttls are
    // already remaining, so pass them through.
    let absolute_ttl = version >= VERSION_ABSOLUTE_TTL;
    let now = kevy_store::now_unix_ms();

    // In-load demotion: the loader drives entries straight
    // into the hot map, so a snapshot bigger than the tier budget must
    // spill inline — check the watermark every K records and drain once
    // more at EOF. A cheap no-op branch when tiering is off.
    let mut records: u64 = 0;
    loop {
        let op = read_u8(&mut r)?;
        if op == OP_EOF {
            store.demote_to_watermark();
            return Ok(());
        }
        records += 1;
        if records.is_multiple_of(crate::REPLAY_DEMOTE_INTERVAL) {
            store.demote_to_watermark();
        }
        // Field-TTL records: no ttl/value framing of their own.
        if op == OP_HFTTL {
            load_hfttl_record(store, &mut r, &keep)?;
            continue;
        }
        let raw_ttl = read_ttl(&mut r)?;
        let ttl = if absolute_ttl {
            raw_ttl.map(|deadline| deadline.saturating_sub(now))
        } else {
            raw_ttl
        };
        let key = read_bytes(&mut r)?;
        load_record(store, &mut r, op, version, key, ttl, &keep)?;
    }
}

/// Validate the magic + version header, skipping the (loader-side
/// advisory) v5+ feed cursor — restore tooling reads it via
/// [`read_snapshot_cursor`]. Returns the format version.
fn read_snapshot_header<R: Read>(r: &mut R) -> io::Result<u8> {
    let mut magic = [0u8; 8];
    r.read_exact(&mut magic)?;
    if &magic != MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "kevy snapshot: bad magic",
        ));
    }
    let version = read_u8(r)?;
    if !(VERSION_RELATIVE_TTL..=VERSION_SEG_STUB).contains(&version) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "kevy snapshot: bad version",
        ));
    }
    if version >= VERSION_FEED_CURSOR {
        let mut cur = [0u8; 16];
        r.read_exact(&mut cur)?;
    }
    Ok(version)
}

/// One `OP_SEGSTUB` record: `[seq u32 LE][value_weight u32 LE]` — the
/// row re-enters the map as a seg-backed stub.
fn load_segstub_record<R: Read>(
    store: &mut Store,
    r: &mut R,
    key: Vec<u8>,
    keep: &impl Fn(&[u8]) -> bool,
) -> io::Result<()> {
    let mut seq = [0u8; 4];
    r.read_exact(&mut seq)?;
    let mut weight = [0u8; 4];
    r.read_exact(&mut weight)?;
    if keep(&key) {
        store.load_row_stub(key, u32::from_le_bytes(seq), u32::from_le_bytes(weight));
    }
    Ok(())
}

/// One `OP_HFTTL` record: `[key][field][deadline_ms: u64 LE]`.
fn load_hfttl_record<R: Read>(
    store: &mut Store,
    r: &mut R,
    keep: &impl Fn(&[u8]) -> bool,
) -> io::Result<()> {
    let key = read_bytes(r)?;
    let field = read_bytes(r)?;
    let mut d = [0u8; 8];
    r.read_exact(&mut d)?;
    if keep(&key) {
        store.load_hash_field_ttl(&key, &field, u64::from_le_bytes(d));
    }
    Ok(())
}

/// Parse one value record's payload and insert it (when `keep(key)`).
/// Skipped records are still fully parsed to stay in frame.
// LOC-WAIVER: pure data-driven snapshot opcode dispatch — one
// load-call arm per opcode, no control flow beyond per-arm framing.
fn load_record<R: Read>(
    store: &mut Store,
    r: &mut R,
    op: u8,
    version: u8,
    key: Vec<u8>,
    ttl: Option<u64>,
    keep: &impl Fn(&[u8]) -> bool,
) -> io::Result<()> {
    match op {
        OP_STR => {
            let val = read_bytes(r)?;
            if keep(&key) {
                store.load_str(key, val, ttl);
            }
        }
        OP_HASH => {
            let fields = read_pair_vec(r)?;
            if keep(&key) {
                store.load_hash(key, fields, ttl);
            }
        }
        OP_LIST => {
            let items = read_bulk_vec(r)?;
            if keep(&key) {
                store.load_list(key, items, ttl);
            }
        }
        OP_SET => {
            let members = read_bulk_vec(r)?;
            if keep(&key) {
                store.load_set(key, members, ttl);
            }
        }
        OP_ZSET => {
            let pairs = read_zset_pairs(r)?;
            if keep(&key) {
                store.load_zset(key, pairs, ttl);
            }
        }
        OP_STREAM => load_stream_record(store, r, version, key, ttl, keep)?,
        OP_SEGSTUB => load_segstub_record(store, r, key, keep)?,
        other => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("kevy snapshot: unknown opcode {other}"),
            ));
        }
    }
    Ok(())
}

/// One `OP_STREAM` payload: scalars, entries, and (v4+) the
/// consumer-group section — v2/v3 files predate groups-in-snapshot,
/// so they load with none.
fn load_stream_record<R: Read>(
    store: &mut Store,
    r: &mut R,
    version: u8,
    key: Vec<u8>,
    ttl: Option<u64>,
    keep: &impl Fn(&[u8]) -> bool,
) -> io::Result<()> {
    let last_ms = read_u64(r)?;
    let last_seq = read_u64(r)?;
    let mxd_ms = read_u64(r)?;
    let mxd_seq = read_u64(r)?;
    let entries_added = read_u64(r)?;
    let n = read_u32(r)? as usize;
    let mut entries = Vec::with_capacity(capped_capacity(n));
    for _ in 0..n {
        let ms = read_u64(r)?;
        let seq = read_u64(r)?;
        let fv = read_pair_vec(r)?;
        entries.push((ms, seq, fv));
    }
    let groups = if version >= VERSION {
        read_stream_groups(r)?
    } else {
        Vec::new()
    };
    if keep(&key) {
        store.load_stream(
            key,
            entries,
            (last_ms, last_seq),
            (mxd_ms, mxd_seq),
            entries_added,
            groups,
            ttl,
        );
    }
    Ok(())
}

/// `[len: u32 LE][bulk]*` — list items / set members.
fn read_bulk_vec<R: Read>(r: &mut R) -> io::Result<Vec<Vec<u8>>> {
    let n = read_u32(r)? as usize;
    let mut items = Vec::with_capacity(capped_capacity(n));
    for _ in 0..n {
        items.push(read_bytes(r)?);
    }
    Ok(items)
}

/// `[len: u32 LE][bulk, bulk]*` — hash fields / stream entry field-values.
fn read_pair_vec<R: Read>(r: &mut R) -> io::Result<Vec<(Vec<u8>, Vec<u8>)>> {
    let n = read_u32(r)? as usize;
    let mut pairs = Vec::with_capacity(capped_capacity(n));
    for _ in 0..n {
        let f = read_bytes(r)?;
        let v = read_bytes(r)?;
        pairs.push((f, v));
    }
    Ok(pairs)
}

/// `[len: u32 LE][bulk, f64-bits u64 LE]*` — zset (member, score) pairs.
fn read_zset_pairs<R: Read>(r: &mut R) -> io::Result<Vec<(Vec<u8>, f64)>> {
    let n = read_u32(r)? as usize;
    let mut pairs = Vec::with_capacity(capped_capacity(n));
    for _ in 0..n {
        let m = read_bytes(r)?;
        let score = f64::from_bits(read_u64(r)?);
        pairs.push((m, score));
    }
    Ok(pairs)
}

/// Loader-side twin of [`crate::snapshot_write::write_stream_groups`].
fn read_stream_groups<R: Read>(r: &mut R) -> io::Result<Vec<kevy_store::LoadedGroup>> {
    let n = read_u32(r)? as usize;
    let mut groups = Vec::with_capacity(capped_capacity(n));
    for _ in 0..n {
        let name = read_bytes(r)?;
        let last_delivered = (read_u64(r)?, read_u64(r)?);
        let nc = read_u32(r)? as usize;
        let mut consumers = Vec::with_capacity(capped_capacity(nc));
        for _ in 0..nc {
            let cname = read_bytes(r)?;
            consumers.push((cname, read_u64(r)?));
        }
        let np = read_u32(r)? as usize;
        let mut pel = Vec::with_capacity(capped_capacity(np));
        for _ in 0..np {
            let ms = read_u64(r)?;
            let seq = read_u64(r)?;
            let consumer = read_bytes(r)?;
            let delivery_time_ms = read_u64(r)?;
            let delivery_count = read_u32(r)?;
            pel.push((ms, seq, consumer, delivery_time_ms, delivery_count));
        }
        groups.push(kevy_store::LoadedGroup { name, last_delivered, consumers, pel });
    }
    Ok(groups)
}
