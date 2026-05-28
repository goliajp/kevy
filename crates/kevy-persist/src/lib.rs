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

pub use kevy_resp::Argv;
use kevy_store::{Store, Value};
// ZSet snapshot iterates ordered (member, score) pairs via `Value::ZSet`.
use std::fs::{File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

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

// ---- AOF (append-only file) ------------------------------------------------

/// When to fsync the AOF to disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fsync {
    /// fsync after every write — safest, slowest.
    Always,
    /// fsync at most once per second (call [`Aof::maybe_sync`] periodically).
    EverySec,
    /// Never fsync explicitly; leave it to the OS.
    No,
}

/// An append-only command log. Each write command is appended as a RESP
/// multi-bulk frame; [`replay_aof`] re-applies them on startup.
///
/// Durability model (paired with snapshots): a snapshot taken at T0 plus the
/// AOF of writes in (T0, now] reconstructs the current state. `SAVE` writes the
/// snapshot then [`Aof::truncate`]s the log, so replay never double-applies.
///
/// Sizes (`size_bytes`, `size_at_last_rewrite`) drive auto-trigger of
/// [`Aof::rewrite_from`] (BGREWRITEAOF) via the
/// `auto_aof_rewrite_percentage` + `auto_aof_rewrite_min_size` knobs in
/// `kevy_config`.
pub struct Aof {
    file: BufWriter<File>,
    path: PathBuf,
    fsync: Fsync,
    dirty: bool,
    last_sync: Instant,
    /// Estimated bytes currently in the AOF file (existing + appended since
    /// open). Maintained without fstat() syscalls per append.
    size_bytes: u64,
    /// File size right after the most recent [`Self::rewrite_from`] (or
    /// `Self::open` if never rewritten). Anchor for `auto_aof_rewrite_*`.
    size_at_last_rewrite: u64,
    /// Total rewrites successfully completed since open. Surfaced via INFO.
    rewrites_total: u64,
}

/// Result of an [`Aof::rewrite_from`] call. Surfaced by `BGREWRITEAOF` /
/// `INFO persistence`.
#[derive(Debug, Clone, Copy)]
pub struct RewriteStats {
    /// Keys dumped into the new AOF.
    pub keys: u64,
    /// New AOF size in bytes.
    pub bytes: u64,
}

impl Aof {
    /// Open (creating if needed) `path` for appending.
    pub fn open(path: &Path, fsync: Fsync) -> io::Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        let size = file.metadata().map(|m| m.len()).unwrap_or(0);
        Ok(Aof {
            file: BufWriter::new(file),
            path: path.to_path_buf(),
            fsync,
            dirty: false,
            last_sync: Instant::now(),
            size_bytes: size,
            size_at_last_rewrite: size,
            rewrites_total: 0,
        })
    }

    /// Append one command, applying the fsync policy.
    pub fn append(&mut self, args: &Argv) -> io::Result<()> {
        write_multibulk(&mut self.file, args)?;
        self.size_bytes = self.size_bytes.saturating_add(estimate_multibulk_bytes(args));
        match self.fsync {
            Fsync::Always => {
                self.file.flush()?;
                self.file.get_ref().sync_data()?;
            }
            Fsync::EverySec | Fsync::No => self.dirty = true,
        }
        Ok(())
    }

    /// Flush+fsync if the `EverySec` window has elapsed. Call once per loop tick.
    pub fn maybe_sync(&mut self) -> io::Result<()> {
        if matches!(self.fsync, Fsync::EverySec)
            && self.dirty
            && self.last_sync.elapsed() >= Duration::from_secs(1)
        {
            self.file.flush()?;
            self.file.get_ref().sync_data()?;
            self.dirty = false;
            self.last_sync = Instant::now();
        }
        Ok(())
    }

    /// Empty the log (after a snapshot has captured the full state).
    pub fn truncate(&mut self) -> io::Result<()> {
        self.file.flush()?;
        let f = self.file.get_mut();
        f.set_len(0)?;
        f.seek(SeekFrom::Start(0))?; // harmless under O_APPEND; keeps len/pos coherent
        f.sync_all()?;
        self.dirty = false;
        self.size_bytes = 0;
        self.size_at_last_rewrite = 0;
        Ok(())
    }

    /// Estimated current AOF size in bytes (file content as of last append).
    #[inline]
    pub fn size_bytes(&self) -> u64 {
        self.size_bytes
    }

    /// AOF size at the most recent rewrite (or open). Auto-trigger compares
    /// `(size_bytes - size_at_last_rewrite) * 100 / size_at_last_rewrite` to
    /// the `auto_aof_rewrite_percentage` knob.
    #[inline]
    pub fn size_at_last_rewrite(&self) -> u64 {
        self.size_at_last_rewrite
    }

    /// Successful rewrite count since `Self::open`. Surfaced in INFO.
    #[inline]
    pub fn rewrites_total(&self) -> u64 {
        self.rewrites_total
    }

    /// BGREWRITEAOF: rebuild a compact AOF from `store`'s current state and
    /// atomically swap it in.
    ///
    /// **v1.0 is synchronous** — the calling shard blocks for the rewrite's
    /// duration. Each shard owns its own AOF, so the shards' rewrites
    /// proceed independently; per-shard blocking matches Redis's `BGSAVE`
    /// cost in a typical single-key-per-shard workload. Concurrent
    /// (rewrite-during-writes) incrementalisation is a v1.x perf item.
    ///
    /// Writes to a `<path>.rewrite` temp file with fsync, then `rename(2)`s
    /// it over the live AOF. The append handle is reopened against the new
    /// file before this call returns, so subsequent `append` calls land in
    /// the rewritten log.
    pub fn rewrite_from(&mut self, store: &Store) -> io::Result<RewriteStats> {
        // Flush any pending writes to the OLD file first so the snapshot
        // accounts for everything the caller intended to durabilise.
        self.file.flush()?;

        let tmp = rewrite_tmp_path(&self.path);
        let (keys, bytes) = dump_store_to_aof(&tmp, store)?;

        // Atomic replacement. After this, the OLD file descriptor in
        // `self.file` is open against an unlinked inode; new writes would
        // go nowhere visible. Reopen against the new path.
        std::fs::rename(&tmp, &self.path)?;
        let f = OpenOptions::new().append(true).open(&self.path)?;
        self.file = BufWriter::new(f);
        self.size_bytes = bytes;
        self.size_at_last_rewrite = bytes;
        self.dirty = false;
        self.rewrites_total = self.rewrites_total.saturating_add(1);
        Ok(RewriteStats { keys, bytes })
    }
}

/// `<aof>.rewrite` — same-directory temp path so `rename(2)` stays atomic.
fn rewrite_tmp_path(path: &Path) -> PathBuf {
    let mut p = path.to_path_buf();
    let new_name = match path.file_name() {
        Some(n) => {
            let mut s = n.to_os_string();
            s.push(".rewrite");
            s
        }
        None => std::ffi::OsString::from("aof.rewrite"),
    };
    p.set_file_name(new_name);
    p
}

/// Write `store`'s current state to `path` as a sequence of mutating RESP
/// commands; flush + fsync before returning. Returns `(keys, bytes)`.
fn dump_store_to_aof(path: &Path, store: &Store) -> io::Result<(u64, u64)> {
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
    let inner = w.into_inner().map_err(|e| {
        io::Error::new(io::ErrorKind::Other, e.to_string())
    })?;
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
fn estimate_multibulk_bytes(args: &Argv) -> u64 {
    let mut n: u64 = 3 + decimal_digits(args.len() as u64) as u64;
    for a in args.iter() {
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

/// Replay the command log at `path`, calling `apply` for each complete command.
/// A truncated or corrupt trailing frame (e.g. a crash mid-append) is ignored.
/// A missing file is treated as an empty log.
pub fn replay_aof<F: FnMut(Argv)>(path: &Path, mut apply: F) -> io::Result<()> {
    let mut data = Vec::new();
    match File::open(path) {
        Ok(mut f) => {
            f.read_to_end(&mut data)?;
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    }
    let mut pos = 0;
    while pos < data.len() {
        match kevy_resp::parse_command(&data[pos..]) {
            Ok(Some((args, consumed))) => {
                apply(args);
                pos += consumed;
            }
            // Incomplete or corrupt tail — stop; the prefix is intact.
            Ok(None) | Err(_) => break,
        }
    }
    Ok(())
}

fn write_multibulk<W: Write>(w: &mut W, args: &Argv) -> io::Result<()> {
    write!(w, "*{}\r\n", args.len())?;
    for a in args.iter() {
        write!(w, "${}\r\n", a.len())?;
        w.write_all(a)?;
        w.write_all(b"\r\n")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn temp_file(name: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        p.push(format!("kevy-{name}-{uniq}.rdb"));
        p
    }

    #[test]
    fn snapshot_round_trip() {
        let path = temp_file("rt");

        let mut src = Store::new();
        src.set(b"plain", b"value".to_vec(), None, false, false);
        src.set(b"empty", Vec::new(), None, false, false);
        src.set(b"binary", vec![0u8, 1, 2, 255, 254], None, false, false);
        src.set(
            b"withttl",
            b"soon".to_vec(),
            Some(Duration::from_secs(100)),
            false,
            false,
        );

        save_snapshot(&src, &path).unwrap();

        let mut dst = Store::new();
        load_snapshot(&mut dst, &path).unwrap();

        assert_eq!(dst.dbsize(), 4);
        assert_eq!(dst.get(b"plain").unwrap(), Some(&b"value"[..]));
        assert_eq!(dst.get(b"empty").unwrap(), Some(&b""[..]));
        assert_eq!(
            dst.get(b"binary").unwrap(),
            Some(&[0u8, 1, 2, 255, 254][..])
        );
        assert_eq!(dst.get(b"withttl").unwrap(), Some(&b"soon"[..]));
        // TTL survived (stored as remaining-ms, restored as a fresh deadline).
        assert!(dst.pttl(b"withttl") > 90_000);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn bad_magic_is_rejected() {
        let path = temp_file("bad");
        std::fs::write(&path, b"NOTKEVY!....").unwrap();
        let mut dst = Store::new();
        assert!(load_snapshot(&mut dst, &path).is_err());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn expired_keys_are_not_saved() {
        let path = temp_file("exp");
        let mut src = Store::new();
        src.set(b"live", b"1".to_vec(), None, false, false);
        src.set(
            b"dead",
            b"2".to_vec(),
            Some(Duration::from_millis(1)),
            false,
            false,
        );
        std::thread::sleep(Duration::from_millis(8));

        save_snapshot(&src, &path).unwrap();
        let mut dst = Store::new();
        load_snapshot(&mut dst, &path).unwrap();

        assert_eq!(dst.dbsize(), 1);
        assert_eq!(dst.get(b"live").unwrap(), Some(&b"1"[..]));
        assert_eq!(dst.get(b"dead").unwrap(), None);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn hash_snapshot_round_trip() {
        let path = temp_file("hashrt");
        let mut src = Store::new();
        src.hset(
            b"h",
            &[
                (b"a".to_vec(), b"1".to_vec()),
                (b"b".to_vec(), b"two".to_vec()),
            ],
        )
        .unwrap();
        src.set(b"s", b"str".to_vec(), None, false, false);
        save_snapshot(&src, &path).unwrap();

        let mut dst = Store::new();
        load_snapshot(&mut dst, &path).unwrap();
        assert_eq!(dst.type_of(b"h"), "hash");
        assert_eq!(dst.hget(b"h", b"a").unwrap(), Some(&b"1"[..]));
        assert_eq!(dst.hget(b"h", b"b").unwrap(), Some(&b"two"[..]));
        assert_eq!(dst.hlen(b"h"), Ok(2));
        assert_eq!(dst.get(b"s").unwrap(), Some(&b"str"[..]));
        let _ = std::fs::remove_file(&path);
    }

    fn cmd(parts: &[&[u8]]) -> Argv {
        Argv::from(parts.iter().map(|p| p.to_vec()).collect::<Vec<_>>())
    }

    #[test]
    fn aof_append_and_replay() {
        let path = temp_file("aof");
        {
            let mut aof = Aof::open(&path, Fsync::Always).unwrap();
            aof.append(&cmd(&[b"SET", b"a", b"1"])).unwrap();
            aof.append(&cmd(&[b"INCR", b"a"])).unwrap();
            aof.append(&cmd(&[b"SET", b"b", b"hello world"])).unwrap();
        }
        let mut got: Vec<Argv> = Vec::new();
        replay_aof(&path, |args| got.push(args)).unwrap();
        assert_eq!(got.len(), 3);
        assert_eq!(got[0], cmd(&[b"SET", b"a", b"1"]));
        assert_eq!(got[1], cmd(&[b"INCR", b"a"]));
        assert_eq!(got[2], cmd(&[b"SET", b"b", b"hello world"]));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn aof_truncated_tail_ignored() {
        let path = temp_file("aoftail");
        {
            let mut aof = Aof::open(&path, Fsync::No).unwrap();
            aof.append(&cmd(&[b"SET", b"a", b"1"])).unwrap();
        }
        // Simulate a crash mid-append: a partial frame at the end.
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(b"*2\r\n$3\r\nSET\r\n$5\r\nhal").unwrap(); // truncated
        drop(f);

        let mut got: Vec<Argv> = Vec::new();
        replay_aof(&path, |args| got.push(args)).unwrap();
        assert_eq!(got, vec![cmd(&[b"SET", b"a", b"1"])]); // only the complete frame
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn aof_truncate_clears() {
        let path = temp_file("aoftrunc");
        let mut aof = Aof::open(&path, Fsync::No).unwrap();
        aof.append(&cmd(&[b"SET", b"a", b"1"])).unwrap();
        aof.truncate().unwrap();
        aof.append(&cmd(&[b"SET", b"b", b"2"])).unwrap();
        drop(aof);

        let mut got: Vec<Argv> = Vec::new();
        replay_aof(&path, |args| got.push(args)).unwrap();
        assert_eq!(got, vec![cmd(&[b"SET", b"b", b"2"])]); // pre-truncate write gone
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn replay_missing_file_is_ok() {
        let path = temp_file("nofile");
        let mut n = 0;
        replay_aof(&path, |_| n += 1).unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn list_snapshot_round_trip() {
        let path = temp_file("listrt");
        let mut src = Store::new();
        src.rpush(b"l", &[b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]).unwrap();
        save_snapshot(&src, &path).unwrap();

        let mut dst = Store::new();
        load_snapshot(&mut dst, &path).unwrap();
        assert_eq!(dst.type_of(b"l"), "list");
        assert_eq!(dst.llen(b"l"), Ok(3));
        assert_eq!(dst.lrange(b"l", 0, -1).unwrap(), vec![
            b"a".to_vec(), b"b".to_vec(), b"c".to_vec()
        ]);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn set_snapshot_round_trip() {
        let path = temp_file("setrt");
        let mut src = Store::new();
        src.sadd(b"s", &[b"x".to_vec(), b"y".to_vec(), b"z".to_vec()]).unwrap();
        save_snapshot(&src, &path).unwrap();

        let mut dst = Store::new();
        load_snapshot(&mut dst, &path).unwrap();
        assert_eq!(dst.type_of(b"s"), "set");
        assert_eq!(dst.scard(b"s"), Ok(3));
        let mut members = dst.smembers(b"s").unwrap();
        members.sort();
        assert_eq!(members, vec![b"x".to_vec(), b"y".to_vec(), b"z".to_vec()]);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn zset_snapshot_round_trip() {
        let path = temp_file("zsetrt");
        let mut src = Store::new();
        src.zadd(b"z", &[(1.0, b"a".to_vec()), (2.0, b"b".to_vec()), (0.5, b"c".to_vec())]).unwrap();
        save_snapshot(&src, &path).unwrap();

        let mut dst = Store::new();
        load_snapshot(&mut dst, &path).unwrap();
        assert_eq!(dst.type_of(b"z"), "zset");
        assert_eq!(dst.zcard(b"z"), Ok(3));
        // Ascending score order: c(0.5), a(1.0), b(2.0)
        let range = dst.zrange(b"z", 0, -1).unwrap();
        assert_eq!(range, vec![
            (b"c".to_vec(), 0.5),
            (b"a".to_vec(), 1.0),
            (b"b".to_vec(), 2.0),
        ]);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn all_types_snapshot_round_trip() {
        let path = temp_file("allrt");
        let mut src = Store::new();
        src.set(b"str", b"hello".to_vec(), None, false, false);
        src.hset(b"hash", &[(b"f".to_vec(), b"v".to_vec())]).unwrap();
        src.rpush(b"list", &[b"i".to_vec()]).unwrap();
        src.sadd(b"set", &[b"m".to_vec()]).unwrap();
        src.zadd(b"zset", &[(1.0, b"k".to_vec())]).unwrap();
        save_snapshot(&src, &path).unwrap();

        let mut dst = Store::new();
        load_snapshot(&mut dst, &path).unwrap();
        assert_eq!(dst.dbsize(), 5);
        assert_eq!(dst.type_of(b"str"), "string");
        assert_eq!(dst.type_of(b"hash"), "hash");
        assert_eq!(dst.type_of(b"list"), "list");
        assert_eq!(dst.type_of(b"set"), "set");
        assert_eq!(dst.type_of(b"zset"), "zset");
        let _ = std::fs::remove_file(&path);
    }

    // ───────────── AOF rewrite (Wave 2 #3) ─────────────

    /// Tiny dispatch helper for AOF-rewrite roundtrip tests: turn the
    /// canonical mutating verbs the rewriter emits back into Store mutations.
    /// Mirrors a subset of `kevy::dispatch` — enough for the verbs
    /// `dump_store_to_aof` actually emits.
    fn apply_for_test(store: &mut Store, args: &Argv) {
        let verb = args[0].to_ascii_uppercase();
        match verb.as_slice() {
            b"SET" => {
                store.set(&args[1], args[2].to_vec(), None, false, false);
            }
            b"HSET" => {
                let mut pairs: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
                let mut i = 2;
                while i + 1 < args.len() {
                    pairs.push((args[i].to_vec(), args[i + 1].to_vec()));
                    i += 2;
                }
                store.hset(&args[1], &pairs).unwrap();
            }
            b"RPUSH" => {
                let items: Vec<Vec<u8>> = args.iter().skip(2).map(|a| a.to_vec()).collect();
                store.rpush(&args[1], &items).unwrap();
            }
            b"SADD" => {
                let members: Vec<Vec<u8>> = args.iter().skip(2).map(|a| a.to_vec()).collect();
                store.sadd(&args[1], &members).unwrap();
            }
            b"ZADD" => {
                let mut pairs: Vec<(f64, Vec<u8>)> = Vec::new();
                let mut i = 2;
                while i + 1 < args.len() {
                    let score: f64 = std::str::from_utf8(&args[i]).unwrap().parse().unwrap();
                    pairs.push((score, args[i + 1].to_vec()));
                    i += 2;
                }
                store.zadd(&args[1], &pairs).unwrap();
            }
            b"PEXPIRE" => {
                let ms: u64 = std::str::from_utf8(&args[2]).unwrap().parse().unwrap();
                store.expire(&args[1], Duration::from_millis(ms));
            }
            other => panic!("unexpected verb in AOF rewrite: {:?}", String::from_utf8_lossy(other)),
        }
    }

    fn temp_aof(name: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        p.push(format!("kevy-{name}-{uniq}.aof"));
        p
    }

    #[test]
    fn rewrite_reconstructs_full_keyspace() {
        let path = temp_aof("rewrite-all");

        let mut src = Store::new();
        src.set(b"str", b"hello".to_vec(), None, false, false);
        src.set(b"binary", vec![0u8, 1, 2, 255], None, false, false);
        src.hset(b"hash", &[(b"f1".to_vec(), b"v1".to_vec()), (b"f2".to_vec(), b"v2".to_vec())])
            .unwrap();
        src.rpush(b"list", &[b"i1".to_vec(), b"i2".to_vec(), b"i3".to_vec()])
            .unwrap();
        src.sadd(b"set", &[b"m1".to_vec(), b"m2".to_vec()]).unwrap();
        src.zadd(b"zset", &[(1.5, b"a".to_vec()), (2.5, b"b".to_vec())])
            .unwrap();
        src.set(
            b"ttl",
            b"x".to_vec(),
            Some(Duration::from_secs(3600)),
            false,
            false,
        );

        let mut aof = Aof::open(&path, Fsync::Always).unwrap();
        let stats = aof.rewrite_from(&src).unwrap();
        assert_eq!(stats.keys, 7);
        assert!(stats.bytes > 0);
        assert_eq!(aof.size_bytes(), stats.bytes);
        assert_eq!(aof.size_at_last_rewrite(), stats.bytes);
        assert_eq!(aof.rewrites_total(), 1);
        drop(aof);

        // Replay into a fresh store; both should match.
        let mut dst = Store::new();
        replay_aof(&path, |args| apply_for_test(&mut dst, &args)).unwrap();
        assert_eq!(dst.dbsize(), 7);
        assert_eq!(dst.get(b"str").unwrap(), Some(&b"hello"[..]));
        assert_eq!(dst.get(b"binary").unwrap(), Some(&[0u8, 1, 2, 255][..]));
        assert_eq!(dst.hget(b"hash", b"f1").unwrap(), Some(&b"v1"[..]));
        assert_eq!(dst.hget(b"hash", b"f2").unwrap(), Some(&b"v2"[..]));
        assert_eq!(dst.llen(b"list").unwrap(), 3);
        assert_eq!(dst.scard(b"set").unwrap(), 2);
        assert_eq!(dst.zcard(b"zset").unwrap(), 2);
        assert!(dst.pttl(b"ttl") > 3_500_000); // TTL survived
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn rewrite_replaces_old_log_atomically() {
        let path = temp_aof("rewrite-swap");

        // Step 1: a stale AOF with many entries (simulating long-running
        // history). After rewrite the new AOF must NOT carry these.
        {
            let mut aof = Aof::open(&path, Fsync::Always).unwrap();
            for i in 0..50 {
                let k = format!("k{i}");
                let argv = Argv::from(vec![b"SET".to_vec(), k.into_bytes(), b"v".to_vec()]);
                aof.append(&argv).unwrap();
            }
        }
        let big_size = std::fs::metadata(&path).unwrap().len();
        assert!(big_size > 0);

        // Step 2: in-memory state is small (only 2 keys).
        let mut store = Store::new();
        store.set(b"only", b"value".to_vec(), None, false, false);
        store.set(b"second", b"v2".to_vec(), None, false, false);
        let mut aof = Aof::open(&path, Fsync::Always).unwrap();
        let stats = aof.rewrite_from(&store).unwrap();
        assert_eq!(stats.keys, 2);
        let new_size = std::fs::metadata(&path).unwrap().len();
        assert!(new_size < big_size, "rewrite should shrink: {new_size} vs {big_size}");

        // Step 3: appending after rewrite lands in the new file.
        aof.append(&Argv::from(vec![b"SET".to_vec(), b"third".to_vec(), b"v".to_vec()]))
            .unwrap();
        drop(aof);

        let mut dst = Store::new();
        replay_aof(&path, |args| apply_for_test(&mut dst, &args)).unwrap();
        assert_eq!(dst.dbsize(), 3, "rewrite + append should yield 3 keys");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn append_bumps_size_estimate() {
        let path = temp_aof("size-est");
        let mut aof = Aof::open(&path, Fsync::No).unwrap();
        assert_eq!(aof.size_bytes(), 0);
        aof.append(&Argv::from(vec![b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()]))
            .unwrap();
        let after_one = aof.size_bytes();
        assert!(after_one > 0);
        aof.append(&Argv::from(vec![b"SET".to_vec(), b"k2".to_vec(), b"v".to_vec()]))
            .unwrap();
        assert!(aof.size_bytes() > after_one);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn rewrite_resets_size_anchor() {
        let path = temp_aof("size-anchor");
        let mut aof = Aof::open(&path, Fsync::Always).unwrap();
        for _ in 0..10 {
            aof.append(&Argv::from(vec![b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()]))
                .unwrap();
        }
        assert!(aof.size_bytes() > aof.size_at_last_rewrite());
        let store = Store::new();
        let stats = aof.rewrite_from(&store).unwrap();
        // empty store ⇒ empty rewrite
        assert_eq!(stats.keys, 0);
        assert_eq!(aof.size_bytes(), 0);
        assert_eq!(aof.size_at_last_rewrite(), 0);
        assert_eq!(aof.rewrites_total(), 1);
        let _ = std::fs::remove_file(&path);
    }
}
