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
mod replay;
mod rewrite_fmt;
mod shards_meta;

pub use aof::{Aof, Fsync, RewritePlan, RewriteStats};
pub use replay::replay_aof;
pub use shards_meta::{Routing, ShardsMeta, read_shards_meta, write_shards_meta};
pub use kevy_resp::{Argv, ArgvView};
pub(crate) use rewrite_fmt::{
    dump_store_to_aof, dump_store_to_buf, estimate_multibulk_bytes, write_multibulk,
};
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
/// deadline, so a load reconstructs the original instant. The loader still
/// accepts v2 (treated as relative) for backward compatibility.
const MAGIC: &[u8; 8] = b"KEVYSNAP";
const VERSION: u8 = 3;
const VERSION_RELATIVE_TTL: u8 = 2;

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

/// Write a point-in-time snapshot of `store` to `path`, atomically: data is
/// written to `<path>.tmp`, fsynced, then renamed over `path`.
pub fn save_snapshot(store: &Store, path: &Path) -> io::Result<()> {
    let tmp = tmp_path(path);
    {
        let mut w = BufWriter::with_capacity(SNAPSHOT_BUF_CAP, File::create(&tmp)?);
        w.write_all(MAGIC)?;
        w.write_all(&[VERSION])?;
        // `snapshot_each` yields *remaining* ms; v3 persists the absolute
        // Unix-ms deadline (now + remaining) so the TTL survives a restart.
        let now = kevy_store::now_unix_ms();
        // `snapshot_each` is infallible; capture the first write error to surface.
        let mut err: Option<io::Error> = None;
        store.snapshot_each(|key, value, ttl| {
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
        w.get_ref().sync_all()?; // durably on disk before the rename
    }
    std::fs::rename(&tmp, path)
}

/// Load a snapshot from `path` into `store` (entries are inserted, not cleared
/// first — call on a fresh store). Errors on a bad magic/version or truncation.
pub fn load_snapshot(store: &mut Store, path: &Path) -> io::Result<()> {
    let mut r = BufReader::new(File::open(path)?);

    let mut magic = [0u8; 8];
    r.read_exact(&mut magic)?;
    if &magic != MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "kevy snapshot: bad magic",
        ));
    }
    let version = read_u8(&mut r)?;
    if version != VERSION && version != VERSION_RELATIVE_TTL {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "kevy snapshot: bad version",
        ));
    }
    // v3 stores absolute Unix-ms deadlines; convert each to remaining ms
    // against one `now` read so the load is internally consistent. A deadline
    // already past becomes `Some(0)` → loaded then immediately reaped (lazy
    // get / active reaper), matching "expired key is gone". v2 ttls are
    // already remaining, so pass them through.
    let absolute_ttl = version >= VERSION;
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
                store.load_stream(
                    key,
                    entries,
                    (last_ms, last_seq),
                    (mxd_ms, mxd_seq),
                    entries_added,
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
    match value {
        Value::Str(v) => {
            w.write_all(&[OP_STR])?;
            write_ttl(w, ttl)?;
            write_bytes(w, key)?;
            write_bytes(w, v.as_slice())?;
        }
        Value::Hash(h) => {
            w.write_all(&[OP_HASH])?;
            write_ttl(w, ttl)?;
            write_bytes(w, key)?;
            w.write_all(&(h.len() as u32).to_le_bytes())?;
            for (f, v) in h.iter() {
                write_bytes(w, f.as_slice())?;
                write_bytes(w, v)?;
            }
        }
        Value::List(l) => {
            w.write_all(&[OP_LIST])?;
            write_ttl(w, ttl)?;
            write_bytes(w, key)?;
            w.write_all(&(l.len() as u32).to_le_bytes())?;
            for item in l.iter() {
                write_bytes(w, item)?;
            }
        }
        Value::Set(set) => {
            w.write_all(&[OP_SET])?;
            write_ttl(w, ttl)?;
            write_bytes(w, key)?;
            w.write_all(&(set.len() as u32).to_le_bytes())?;
            for m in set.iter() {
                write_bytes(w, m.as_slice())?;
            }
        }
        Value::ZSet(z) => {
            w.write_all(&[OP_ZSET])?;
            write_ttl(w, ttl)?;
            write_bytes(w, key)?;
            let entries: Vec<(&[u8], f64)> = z.ordered().collect();
            w.write_all(&(entries.len() as u32).to_le_bytes())?;
            for (m, score) in entries {
                write_bytes(w, m)?;
                w.write_all(&score.to_bits().to_le_bytes())?;
            }
        }
        Value::Stream(s) => {
            w.write_all(&[OP_STREAM])?;
            write_ttl(w, ttl)?;
            write_bytes(w, key)?;
            w.write_all(&s.last_id().ms.to_le_bytes())?;
            w.write_all(&s.last_id().seq.to_le_bytes())?;
            w.write_all(&s.max_deleted_id().ms.to_le_bytes())?;
            w.write_all(&s.max_deleted_id().seq.to_le_bytes())?;
            w.write_all(&s.entries_added().to_le_bytes())?;
            let len = s.length() as u32;
            w.write_all(&len.to_le_bytes())?;
            for (id, fv) in s.iter_entries() {
                w.write_all(&id.ms.to_le_bytes())?;
                w.write_all(&id.seq.to_le_bytes())?;
                w.write_all(&(fv.len() as u32).to_le_bytes())?;
                for (f, v) in fv {
                    write_bytes(w, f.as_slice())?;
                    write_bytes(w, v.as_slice())?;
                }
            }
        }
    }
    Ok(())
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
