//! kevy-persist — durability for a [`kevy_store::Store`].
//!
//! Two mechanisms, both zero-dependency pure Rust over `std::fs`:
//!
//! - **Snapshot (RDB-style):** [`save_snapshot`] dumps a whole store to a temp
//!   file then atomically renames it (fsync before rename); [`load_snapshot`]
//!   restores it. A compact, type-tagged binary format.
//! - **AOF:** an [`Aof`] append-only command log with a configurable fsync
//!   policy; [`replay_aof`] re-applies it on startup, tolerating a truncated
//!   trailing frame from a crash mid-write.
//!
//! In a shared-nothing runtime each shard persists its own store to its own
//! file, so there is no cross-core coordination. Part of the [kevy] server.
//!
//! [kevy]: https://crates.io/crates/kevy
//!
//! # Example (AOF)
//!
//! ```
//! use kevy_persist::{Aof, Argv, Fsync, replay_aof};
//!
//! # fn main() -> std::io::Result<()> {
//! let path = std::env::temp_dir().join("kevy-persist-doctest.aof");
//! # let _ = std::fs::remove_file(&path);
//! {
//!     let mut aof = Aof::open(&path, Fsync::No)?;
//!     aof.append(&Argv::from(vec![b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()]))?;
//! } // flushed on drop
//!
//! let mut replayed: Vec<Argv> = Vec::new();
//! replay_aof(&path, |args| replayed.push(args))?;
//! assert_eq!(replayed, vec![vec![b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()]]);
//! # std::fs::remove_file(&path).ok();
//! # Ok(())
//! # }
//! ```
#![forbid(unsafe_code)]

mod aof;
pub mod layout;
mod replay;
pub mod reshard;
mod rewrite_fmt;
mod shards_meta;

pub use aof::{Aof, Fsync, RewritePlan, RewriteStats, write_aof_base};
pub use replay::replay_aof;
pub use shards_meta::{Routing, ShardsMeta, read_shards_meta, write_shards_meta};
pub use kevy_resp::{Argv, ArgvView};
pub use rewrite_fmt::dump_aof;
pub(crate) use rewrite_fmt::{dump_store_to_buf, estimate_multibulk_bytes, write_multibulk};
use kevy_store::Store;
use kevy_store::Value;
// ZSet snapshot iterates ordered (member, score) pairs via `Value::ZSet`.
use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::Path;

/// File magic + format version. Bump `VERSION` on any layout change.
///
/// v2 stored each entry's TTL as **remaining millis** (relative), so a load
/// re-anchored the deadline to load-time — a restart reset every key to a
/// fresh full TTL (INC-2026-06-09). v3 stores the **absolute** Unix-ms
/// deadline, so a load reconstructs the original instant. v4 appends a
/// consumer-group section to each `OP_STREAM` payload (groups + consumers
/// plus PEL) — before that, SAVE/reshard silently dropped group state. The
/// loader still accepts v2 (relative TTL) and v3 (no group section).
const MAGIC: &[u8; 8] = b"KEVYSNAP";
const VERSION: u8 = 4;
const VERSION_RELATIVE_TTL: u8 = 2;
const VERSION_ABSOLUTE_TTL: u8 = 3;

// Record opcodes (one per value type). Each record is:
//   [op][ttl: u8 flag + optional u64][key][type payload]
const OP_EOF: u8 = 0;
const OP_STR: u8 = 1;
const OP_HASH: u8 = 2;
const OP_LIST: u8 = 3;
const OP_SET: u8 = 4;
const OP_ZSET: u8 = 5;
const OP_STREAM: u8 = 6;

/// BufWriter capacity for bulk snapshot / AOF-rewrite writes. The 8 KiB
/// default made SAVE ~12 % of disk bandwidth (tens of thousands of small
/// `write(2)`s); 1 MiB amortizes the syscalls toward disk speed.
pub(crate) const SNAPSHOT_BUF_CAP: usize = 1 << 20;

/// Anything that can enumerate `(key, &Value, ttl_ms)` triples for
/// serialization: a live [`Store`] (its `snapshot_each`, the synchronous
/// paths) or a frozen [`kevy_store::SnapshotView`] (the COW paths — collect
/// on the owning thread, serialize on a background one).
pub trait SnapshotSource {
    /// Visit every live entry as `(key, &value, remaining_ttl_ms)`.
    fn for_each_entry(&self, f: impl FnMut(&[u8], &Value, Option<u64>));
}

impl SnapshotSource for Store {
    fn for_each_entry(&self, f: impl FnMut(&[u8], &Value, Option<u64>)) {
        self.snapshot_each(f);
    }
}

impl SnapshotSource for kevy_store::SnapshotView {
    fn for_each_entry(&self, f: impl FnMut(&[u8], &Value, Option<u64>)) {
        self.each(f);
    }
}

/// Write a point-in-time snapshot of `src` (a live [`Store`] or a frozen
/// [`kevy_store::SnapshotView`]) to `path`, atomically: data is written to
/// `<path>.tmp`, fsynced, then renamed over `path`.
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
    let mut w = BufWriter::with_capacity(SNAPSHOT_BUF_CAP, sink);
    w.write_all(MAGIC)?;
    w.write_all(&[VERSION])?;
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
    w.write_all(&[OP_EOF])?;
    w.flush()?;
    Ok(())
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

/// Load a snapshot from `path` into `store` (entries are inserted, not cleared
/// first — call on a fresh store). Errors on a bad magic/version or truncation.
pub fn load_snapshot(store: &mut Store, path: &Path) -> io::Result<()> {
    let r = BufReader::new(File::open(path)?);
    load_snapshot_from(store, r)
}

/// Load a snapshot from any [`std::io::Read`] sink into `store` —
/// symmetric to [`write_snapshot_to`]. Used by the v3-cluster
/// replication path (sink = `&[u8]` wrapped in `std::io::Cursor`) to
/// apply a primary-shipped snapshot to a fresh local store without
/// touching disk. Entries are inserted, not cleared first — call on
/// a fresh store. Errors on bad magic/version or truncation.
pub fn load_snapshot_from<R: Read>(store: &mut Store, mut r: R) -> io::Result<()> {
    let mut magic = [0u8; 8];
    r.read_exact(&mut magic)?;
    if &magic != MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "kevy snapshot: bad magic",
        ));
    }
    let version = read_u8(&mut r)?;
    if !(VERSION_RELATIVE_TTL..=VERSION).contains(&version) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "kevy snapshot: bad version",
        ));
    }
    // v3+ stores absolute Unix-ms deadlines; convert each to remaining ms
    // against one `now` read so the load is internally consistent. A deadline
    // already past becomes `Some(0)` → loaded then immediately reaped (lazy
    // get / active reaper), matching "expired key is gone". v2 ttls are
    // already remaining, so pass them through.
    let absolute_ttl = version >= VERSION_ABSOLUTE_TTL;
    let now = kevy_store::now_unix_ms();

    loop {
        let op = read_u8(&mut r)?;
        if op == OP_EOF {
            return Ok(());
        }
        let raw_ttl = read_ttl(&mut r)?;
        let ttl = if absolute_ttl {
            raw_ttl.map(|deadline| deadline.saturating_sub(now))
        } else {
            raw_ttl
        };
        let key = read_bytes(&mut r)?;
        match op {
            OP_STR => {
                let val = read_bytes(&mut r)?;
                store.load_str(key, val, ttl);
            }
            OP_HASH => {
                let n = read_u32(&mut r)? as usize;
                let mut fields = Vec::with_capacity(n);
                for _ in 0..n {
                    let f = read_bytes(&mut r)?;
                    let v = read_bytes(&mut r)?;
                    fields.push((f, v));
                }
                store.load_hash(key, fields, ttl);
            }
            OP_LIST => {
                let n = read_u32(&mut r)? as usize;
                let mut items = Vec::with_capacity(n);
                for _ in 0..n {
                    items.push(read_bytes(&mut r)?);
                }
                store.load_list(key, items, ttl);
            }
            OP_SET => {
                let n = read_u32(&mut r)? as usize;
                let mut members = Vec::with_capacity(n);
                for _ in 0..n {
                    members.push(read_bytes(&mut r)?);
                }
                store.load_set(key, members, ttl);
            }
            OP_ZSET => {
                let n = read_u32(&mut r)? as usize;
                let mut pairs = Vec::with_capacity(n);
                for _ in 0..n {
                    let m = read_bytes(&mut r)?;
                    let score = f64::from_bits(read_u64(&mut r)?);
                    pairs.push((m, score));
                }
                store.load_zset(key, pairs, ttl);
            }
            OP_STREAM => {
                let last_ms = read_u64(&mut r)?;
                let last_seq = read_u64(&mut r)?;
                let mxd_ms = read_u64(&mut r)?;
                let mxd_seq = read_u64(&mut r)?;
                let entries_added = read_u64(&mut r)?;
                let n = read_u32(&mut r)? as usize;
                let mut entries = Vec::with_capacity(n);
                for _ in 0..n {
                    let ms = read_u64(&mut r)?;
                    let seq = read_u64(&mut r)?;
                    let nf = read_u32(&mut r)? as usize;
                    let mut fv = Vec::with_capacity(nf);
                    for _ in 0..nf {
                        let f = read_bytes(&mut r)?;
                        let v = read_bytes(&mut r)?;
                        fv.push((f, v));
                    }
                    entries.push((ms, seq, fv));
                }
                // v4 appends the consumer-group section; v2/v3 files
                // predate groups-in-snapshot, so they load with none.
                let groups = if version >= VERSION {
                    read_stream_groups(&mut r)?
                } else {
                    Vec::new()
                };
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
            other => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("kevy snapshot: unknown opcode {other}"),
                ));
            }
        }
    }
}

/// Serialize one entry: `[op][ttl][key][payload]`.
fn write_entry<W: Write>(w: &mut W, key: &[u8], value: &Value, ttl: Option<u64>) -> io::Result<()> {
    let op = match value {
        Value::Str(_) | Value::Int(_) | Value::ArcBulk(_) => OP_STR, // L1/L2: all reuse OP_STR.

        Value::Hash(_) => OP_HASH,
        Value::List(_) => OP_LIST,
        Value::Set(_) => OP_SET,
        Value::ZSet(_) => OP_ZSET,
        Value::Stream(_) => OP_STREAM,
    };
    w.write_all(&[op])?;
    write_ttl(w, ttl)?;
    write_bytes(w, key)?;
    match value {
        Value::Str(v) => write_bytes(w, v.as_slice()),
        Value::Int(n) => write_bytes(w, n.to_string().as_bytes()),
        Value::ArcBulk(a) => write_bytes(w, a.as_ref()),
        Value::Hash(h) => write_hash_payload(w, h),
        Value::List(l) => write_list_payload(w, l),
        Value::Set(set) => write_set_payload(w, set),
        Value::ZSet(z) => write_zset_payload(w, z),
        Value::Stream(s) => write_stream_payload(w, s),
    }
}

fn write_hash_payload<W: Write>(w: &mut W, h: &kevy_store::HashData) -> io::Result<()> {
    w.write_all(&(h.len() as u32).to_le_bytes())?;
    for (f, v) in h {
        write_bytes(w, f.as_slice())?;
        write_bytes(w, v)?;
    }
    Ok(())
}

fn write_list_payload<W: Write>(w: &mut W, l: &kevy_store::ListData) -> io::Result<()> {
    w.write_all(&(l.len() as u32).to_le_bytes())?;
    for item in l {
        write_bytes(w, item)?;
    }
    Ok(())
}

fn write_set_payload<W: Write>(w: &mut W, set: &kevy_store::SetData) -> io::Result<()> {
    w.write_all(&(set.len() as u32).to_le_bytes())?;
    for m in set {
        write_bytes(w, m.as_slice())?;
    }
    Ok(())
}

fn write_zset_payload<W: Write>(w: &mut W, z: &kevy_store::ZSetData) -> io::Result<()> {
    let entries: Vec<(&[u8], f64)> = z.ordered().collect();
    w.write_all(&(entries.len() as u32).to_le_bytes())?;
    for (m, score) in entries {
        write_bytes(w, m)?;
        w.write_all(&score.to_bits().to_le_bytes())?;
    }
    Ok(())
}

fn write_stream_payload<W: Write>(w: &mut W, s: &kevy_store::StreamData) -> io::Result<()> {
    w.write_all(&s.last_id().ms.to_le_bytes())?;
    w.write_all(&s.last_id().seq.to_le_bytes())?;
    w.write_all(&s.max_deleted_id().ms.to_le_bytes())?;
    w.write_all(&s.max_deleted_id().seq.to_le_bytes())?;
    w.write_all(&s.entries_added().to_le_bytes())?;
    w.write_all(&(s.length() as u32).to_le_bytes())?;
    for (id, fv) in s.iter_entries() {
        w.write_all(&id.ms.to_le_bytes())?;
        w.write_all(&id.seq.to_le_bytes())?;
        w.write_all(&(fv.len() as u32).to_le_bytes())?;
        for (f, v) in fv {
            write_bytes(w, f.as_slice())?;
            write_bytes(w, v.as_slice())?;
        }
    }
    write_stream_groups(w, &s.export_groups())
}

/// v4 consumer-group section: `[n_groups][per group: name, last_delivered,
/// consumers (name + last_seen_ms), PEL rows]`. Tombstone PEL rows are kept
/// — the snapshot path is the full-fidelity one (the AOF rewrite can't
/// re-create them via XCLAIM, see `rewrite_fmt`).
fn write_stream_groups<W: Write>(w: &mut W, groups: &[kevy_store::LoadedGroup]) -> io::Result<()> {
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

/// Loader-side twin of [`write_stream_groups`].
fn read_stream_groups<R: Read>(r: &mut R) -> io::Result<Vec<kevy_store::LoadedGroup>> {
    let n = read_u32(r)? as usize;
    let mut groups = Vec::with_capacity(n);
    for _ in 0..n {
        let name = read_bytes(r)?;
        let last_delivered = (read_u64(r)?, read_u64(r)?);
        let nc = read_u32(r)? as usize;
        let mut consumers = Vec::with_capacity(nc);
        for _ in 0..nc {
            let cname = read_bytes(r)?;
            consumers.push((cname, read_u64(r)?));
        }
        let np = read_u32(r)? as usize;
        let mut pel = Vec::with_capacity(np);
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

fn write_ttl<W: Write>(w: &mut W, ttl: Option<u64>) -> io::Result<()> {
    match ttl {
        Some(ms) => {
            w.write_all(&[1u8])?;
            w.write_all(&ms.to_le_bytes())?;
        }
        None => w.write_all(&[0u8])?,
    }
    Ok(())
}

fn read_ttl<R: Read>(r: &mut R) -> io::Result<Option<u64>> {
    if read_u8(r)? == 1 {
        Ok(Some(read_u64(r)?))
    } else {
        Ok(None)
    }
}

fn tmp_path(path: &Path) -> std::path::PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".tmp");
    s.into()
}

fn write_bytes<W: Write>(w: &mut W, b: &[u8]) -> io::Result<()> {
    w.write_all(&(b.len() as u32).to_le_bytes())?;
    w.write_all(b)
}

fn read_bytes<R: Read>(r: &mut R) -> io::Result<Vec<u8>> {
    let len = read_u32(r)? as usize;
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    Ok(buf)
}

fn read_u8<R: Read>(r: &mut R) -> io::Result<u8> {
    let mut b = [0u8; 1];
    r.read_exact(&mut b)?;
    Ok(b[0])
}

fn read_u32<R: Read>(r: &mut R) -> io::Result<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_le_bytes(b))
}

fn read_u64<R: Read>(r: &mut R) -> io::Result<u64> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b)?;
    Ok(u64::from_le_bytes(b))
}

#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_aof;
#[cfg(test)]
mod tests_rewrite;
