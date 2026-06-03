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

pub use aof::{Aof, Fsync, RewriteStats};
pub use replay::replay_aof;
pub use kevy_resp::{Argv, ArgvView};
use kevy_store::{Store, Value};
// ZSet snapshot iterates ordered (member, score) pairs via `Value::ZSet`.
use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::Path;

/// File magic + format version. Bump `VERSION` on any layout change.
const MAGIC: &[u8; 8] = b"KEVYSNAP";
const VERSION: u8 = 2;

// Record opcodes (one per value type). Each record is:
//   [op][ttl: u8 flag + optional u64][key][type payload]
const OP_EOF: u8 = 0;
const OP_STR: u8 = 1;
const OP_HASH: u8 = 2;
const OP_LIST: u8 = 3;
const OP_SET: u8 = 4;
const OP_ZSET: u8 = 5;

/// Write a point-in-time snapshot of `store` to `path`, atomically: data is
/// written to `<path>.tmp`, fsynced, then renamed over `path`.
pub fn save_snapshot(store: &Store, path: &Path) -> io::Result<()> {
    let tmp = tmp_path(path);
    {
        let mut w = BufWriter::new(File::create(&tmp)?);
        w.write_all(MAGIC)?;
        w.write_all(&[VERSION])?;
        // `snapshot_each` is infallible; capture the first write error to surface.
        let mut err: Option<io::Error> = None;
        store.snapshot_each(|key, value, ttl| {
            if err.is_none()
                && let Err(e) = write_entry(&mut w, key, value, ttl)
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
    if read_u8(&mut r)? != VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "kevy snapshot: bad version",
        ));
    }

    loop {
        let op = read_u8(&mut r)?;
        if op == OP_EOF {
            return Ok(());
        }
        let ttl = read_ttl(&mut r)?;
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


/// Write `store`'s current state to `path` as a sequence of mutating RESP
/// commands; flush + fsync before returning. Returns `(keys, bytes)`.
pub(crate) fn dump_store_to_aof(path: &Path, store: &Store) -> io::Result<(u64, u64)> {
    let f = File::create(path)?;
    let mut w = BufWriter::new(f);
    let mut keys = 0u64;
    let mut err: Option<io::Error> = None;
    store.snapshot_each(|key, value, ttl_ms| {
        if err.is_some() {
            return;
        }
        if let Err(e) = write_value_as_commands(&mut w, key, value, ttl_ms) {
            err = Some(e);
        } else {
            keys += 1;
        }
    });
    if let Some(e) = err {
        return Err(e);
    }
    w.flush()?;
    let inner = w
        .into_inner()
        .map_err(|e| io::Error::other(e.to_string()))?;
    let bytes = inner.metadata().map(|m| m.len()).unwrap_or(0);
    inner.sync_all()?;
    Ok((keys, bytes))
}

/// Emit one (or two, if TTL'd) RESP write commands that, when replayed,
/// reconstruct `key`'s `value` and TTL exactly.
fn write_value_as_commands<W: Write>(
    w: &mut W,
    key: &[u8],
    value: &Value,
    ttl_ms: Option<u64>,
) -> io::Result<()> {
    match value {
        Value::Str(s) => {
            let argv = Argv::from(vec![b"SET".to_vec(), key.to_vec(), s.to_vec()]);
            write_multibulk(w, &argv)?;
        }
        Value::Hash(h) => {
            let mut argv: Vec<Vec<u8>> = Vec::with_capacity(2 + h.len() * 2);
            argv.push(b"HSET".to_vec());
            argv.push(key.to_vec());
            for (f, v) in h.iter() {
                argv.push(f.to_vec());
                argv.push(v.clone());
            }
            write_multibulk(w, &Argv::from(argv))?;
        }
        Value::List(l) => {
            let mut argv: Vec<Vec<u8>> = Vec::with_capacity(2 + l.len());
            argv.push(b"RPUSH".to_vec());
            argv.push(key.to_vec());
            for v in l.iter() {
                argv.push(v.clone());
            }
            write_multibulk(w, &Argv::from(argv))?;
        }
        Value::Set(s) => {
            let mut argv: Vec<Vec<u8>> = Vec::with_capacity(2 + s.len());
            argv.push(b"SADD".to_vec());
            argv.push(key.to_vec());
            for m in s.iter() {
                argv.push(m.to_vec());
            }
            write_multibulk(w, &Argv::from(argv))?;
        }
        Value::ZSet(z) => {
            let mut argv: Vec<Vec<u8>> = Vec::with_capacity(2 + z.ordered().count() * 2);
            argv.push(b"ZADD".to_vec());
            argv.push(key.to_vec());
            for (m, sc) in z.ordered() {
                argv.push(fmt_zset_score(sc));
                argv.push(m.to_vec());
            }
            write_multibulk(w, &Argv::from(argv))?;
        }
    }
    if let Some(ms) = ttl_ms {
        let argv = Argv::from(vec![
            b"PEXPIRE".to_vec(),
            key.to_vec(),
            ms.to_string().into_bytes(),
        ]);
        write_multibulk(w, &argv)?;
    }
    Ok(())
}

/// Format a sorted-set score the way Redis does (no trailing `.0` for
/// integers; up to 17 sig figs for non-integer doubles). Tests want the
/// replay-roundtrip to compare byte-equal, so don't introduce locale
/// differences (`format!` is locale-free here).
fn fmt_zset_score(s: f64) -> Vec<u8> {
    if s.is_finite() && s == s.trunc() && s.abs() < 1e17 {
        format!("{}", s as i64).into_bytes()
    } else {
        format!("{s:.17}").into_bytes()
    }
}

/// Cheap byte-count estimator for a single multi-bulk frame:
/// `*<n>\r\n` + per-arg `$<len>\r\n<bytes>\r\n`. No allocation, no
/// double-pass — accurate to within a couple of bytes per arg.
pub(crate) fn estimate_multibulk_bytes<A: ArgvView + ?Sized>(args: &A) -> u64 {
    let mut n: u64 = 3 + decimal_digits(args.len() as u64) as u64;
    for i in 0..args.len() {
        let a = &args[i];
        n += 3 + decimal_digits(a.len() as u64) as u64 + a.len() as u64 + 2;
    }
    n
}

#[inline]
fn decimal_digits(mut x: u64) -> u32 {
    if x == 0 {
        return 1;
    }
    let mut d = 0;
    while x > 0 {
        d += 1;
        x /= 10;
    }
    d
}


pub(crate) fn write_multibulk<W: Write, A: ArgvView + ?Sized>(
    w: &mut W,
    args: &A,
) -> io::Result<()> {
    write!(w, "*{}\r\n", args.len())?;
    for i in 0..args.len() {
        let a = &args[i];
        write!(w, "${}\r\n", a.len())?;
        w.write_all(a)?;
        w.write_all(b"\r\n")?;
    }
    Ok(())
}


#[cfg(test)]
mod tests;
