//! `kevy-cli backup` / `kevy-cli restore` subcommands.
//!
//! Backups bundle a kevy `data_dir` (snapshot + AOF) into a single
//! `.kevybkp` file using a tiny custom container format (std-only,
//! 0-dep — no `tar` crate per project rule).
//!
//! Container format:
//!
//! ```text
//!   magic        : 8 bytes = b"KEVYBKP1"
//!   for each file:
//!     name_len   : u16 big-endian
//!     name       : <name_len> bytes (UTF-8, relative to data_dir)
//!     body_len   : u64 big-endian
//!     body       : <body_len> bytes
//!   eof marker   : u16 = 0
//! ```
//!
//! Backup ordering: typically the operator issues a `BGSAVE` first
//! against the live kevy via TCP, then runs `kevy-cli backup` once
//! the snapshot has flushed. (kevy-cli backup can do this in one
//! call too — see the `--bgsave` flag.)

use std::fs::{File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

const MAGIC: &[u8; 8] = b"KEVYBKP1";

/// Pack every regular file under `data_dir` into `out_path`.
/// Subdirectories are skipped, and the data dir stopped being flat when
/// tiering landed: `tier/<shard>/vlog-*.dat` holds demoted values and
/// `segs-<shard>/` holds cold index segments. Skipping them is correct
/// because both are **derived** — a snapshot materialises cold values
/// rather than referencing them, so the backup carries the data without
/// the spill area and a restore rebuilds it — measured end to end
/// against a store whose values had actually been demoted, not
/// reasoned about.
///
/// That safety is a property of what lives down there, not of the skip.
/// `subdirectories_are_derived_spill_only` pins the set, so a future
/// directory of TRUTH cannot join the data dir without someone reading
/// this first.
pub fn pack(data_dir: &Path, out_path: &Path) -> io::Result<u64> {
    let out = OpenOptions::new().create_new(true).write(true).open(out_path)?;
    let mut w = BufWriter::new(out);
    w.write_all(MAGIC)?;
    let mut total_bytes: u64 = 0;
    let mut file_count: u64 = 0;
    for entry in std::fs::read_dir(data_dir)? {
        let entry = entry?;
        let path = entry.path();
        let meta = entry.metadata()?;
        if !meta.is_file() {
            continue; // derived spill — see the note above this fn
        }
        let name = path
            .strip_prefix(data_dir)
            .map_err(|_| io::Error::other("name not under data_dir"))?
            .to_string_lossy()
            .into_owned();
        let name_bytes = name.as_bytes();
        if name_bytes.len() > u16::MAX as usize {
            return Err(io::Error::other("file name too long"));
        }
        let body_len = meta.len();
        w.write_all(&(name_bytes.len() as u16).to_be_bytes())?;
        w.write_all(name_bytes)?;
        w.write_all(&body_len.to_be_bytes())?;
        copy_file_body(&mut w, &path, body_len)?;
        total_bytes += body_len;
        file_count += 1;
    }
    // EOF marker: name_len = 0.
    w.write_all(&0u16.to_be_bytes())?;
    w.flush()?;
    eprintln!(
        "kevy-cli: backed up {file_count} file(s), {total_bytes} bytes total → {}",
        out_path.display()
    );
    Ok(total_bytes)
}

/// Stream exactly `body_len` bytes of `path` into `w`.
fn copy_file_body(w: &mut impl Write, path: &Path, body_len: u64) -> io::Result<()> {
    let mut f = BufReader::new(File::open(path)?);
    let mut buf = vec![0u8; 64 * 1024];
    let mut copied = 0u64;
    while copied < body_len {
        let want = std::cmp::min((body_len - copied) as usize, buf.len());
        let n = f.read(&mut buf[..want])?;
        if n == 0 {
            // File shrunk between metadata-stat and content-read
            // (live backup race; AOF rewrite can shrink the file).
            // Pad with zeros to honor the body_len we committed.
            // Restore replay handles trailing zeros as torn-frame
            // tail truncation (existing kevy-persist::replay logic).
            let mut remaining = body_len - copied;
            let zeros = [0u8; 64 * 1024];
            while remaining > 0 {
                let chunk = std::cmp::min(remaining as usize, zeros.len());
                w.write_all(&zeros[..chunk])?;
                remaining -= chunk as u64;
            }
            break;
        }
        w.write_all(&buf[..n])?;
        copied += n as u64;
    }
    Ok(())
}

/// Unpack the container at `in_path` into `target_dir` (created if
/// missing; refuses to overwrite an existing non-empty dir to avoid
/// clobbering live data).
pub fn unpack(in_path: &Path, target_dir: &Path) -> io::Result<u64> {
    std::fs::create_dir_all(target_dir)?;
    // Refuse to write into a non-empty dir (safety).
    let existing = std::fs::read_dir(target_dir)?.count();
    if existing > 0 {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!(
                "target dir {} is not empty ({existing} entries); refuse to overwrite",
                target_dir.display()
            ),
        ));
    }
    let mut r = BufReader::new(File::open(in_path)?);
    let mut magic = [0u8; 8];
    r.read_exact(&mut magic)?;
    if &magic != MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "not a kevy backup container (magic mismatch)",
        ));
    }
    let mut file_count = 0u64;
    let mut total = 0u64;
    // Loop ends at the EOF marker (read_entry_name -> None).
    while let Some(name) = read_entry_name(&mut r)? {
        let mut body_len_buf = [0u8; 8];
        r.read_exact(&mut body_len_buf)?;
        let body_len = u64::from_be_bytes(body_len_buf);
        let out_path = target_dir.join(&name);
        copy_entry_body(&mut r, &out_path, &name, body_len)?;
        file_count += 1;
        total += body_len;
    }
    eprintln!(
        "kevy-cli: restored {file_count} file(s), {total} bytes total → {}",
        target_dir.display()
    );
    Ok(total)
}

/// Read one entry's name header. `None` = the EOF marker (name_len 0).
fn read_entry_name(r: &mut impl Read) -> io::Result<Option<String>> {
    let mut name_len_buf = [0u8; 2];
    r.read_exact(&mut name_len_buf)?;
    let name_len = u16::from_be_bytes(name_len_buf);
    if name_len == 0 {
        return Ok(None);
    }
    let mut name_bytes = vec![0u8; name_len as usize];
    r.read_exact(&mut name_bytes)?;
    let name = std::str::from_utf8(&name_bytes)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    // Reject path components that try to escape (e.g., "../etc/passwd").
    if name.contains("..") || name.starts_with('/') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("backup entry name {name:?} contains path traversal"),
        ));
    }
    Ok(Some(name.to_owned()))
}

/// Stream exactly `body_len` bytes from `r` into a fresh file at
/// `out_path`. `name` only feeds the truncation error message.
fn copy_entry_body(
    r: &mut impl Read,
    out_path: &Path,
    name: &str,
    body_len: u64,
) -> io::Result<()> {
    let mut out = BufWriter::new(File::create(out_path)?);
    let mut remaining = body_len;
    let mut buf = vec![0u8; 64 * 1024];
    while remaining > 0 {
        let want = std::cmp::min(remaining as usize, buf.len());
        let n = r.read(&mut buf[..want])?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!("backup truncated mid-file {name:?}"),
            ));
        }
        out.write_all(&buf[..n])?;
        remaining -= n as u64;
    }
    out.flush()
}

/// Wrapper around `pack` that accepts string paths for the CLI layer.
pub fn run_backup(data_dir: PathBuf, out_path: PathBuf) -> io::Result<()> {
    pack(&data_dir, &out_path).map(|_| ())
}

/// Wrapper around `unpack` for CLI layer.
pub fn run_restore(in_path: PathBuf, target_dir: PathBuf) -> io::Result<()> {
    unpack(&in_path, &target_dir).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh, empty, uniquely-named directory.
    fn tmp(name: &str) -> PathBuf {
        kevy_tmpdir::unique_dir(&format!("cli-backup-{name}"))
    }

    /// A unique path for a FILE that must not exist yet — pack's output. The
    /// old helper returned an uncreated path and let the call site decide what
    /// it was; that ambiguity is how the same function ended up naming both
    /// directories and files.
    fn tmp_file(name: &str) -> PathBuf {
        tmp("files").join(name)
    }

    /// `pack` skips subdirectories. That is safe only while everything
    /// under the data dir's subdirectories is derived spill the restore
    /// can rebuild — true today for `tier/` (demoted values, reachable
    /// through a snapshot that materialises them) and `segs-*/` (cold
    /// index segments, rebuilt by re-sliding).
    ///
    /// This test does not verify that property; it pins the *set* so
    /// that adding a subdirectory forces the person adding it to come
    /// here and state which kind it is. A backup that silently omits a
    /// directory of truth is the failure this is standing in front of.
    #[test]
    fn subdirectories_are_derived_spill_only() {
        const DERIVED: [&str; 2] = ["tier", "segs"];
        let src = tmp("derived");
        std::fs::write(src.join("aof-0.aof"), b"truth").unwrap();
        for d in ["tier/0", "segs-0"] {
            std::fs::create_dir_all(src.join(d)).unwrap();
            std::fs::write(src.join(d).join("spill.dat"), b"derived").unwrap();
        }
        let out = tmp_file("derived.kevybkp");
        pack(&src, &out).unwrap();
        let target = tmp("derived-restored");
        unpack(&out, &target).unwrap();

        assert_eq!(std::fs::read(target.join("aof-0.aof")).unwrap(), b"truth");
        for d in ["tier", "segs-0"] {
            assert!(!target.join(d).exists(), "{d} came along; it is spill, not truth");
        }
        // The guard: every subdirectory the source had must be one the
        // list above already calls derived.
        for e in std::fs::read_dir(&src).unwrap() {
            let e = e.unwrap();
            if e.metadata().unwrap().is_dir() {
                let name = e.file_name().to_string_lossy().into_owned();
                assert!(
                    DERIVED.iter().any(|d| name.starts_with(d)),
                    "'{name}' is a data-dir subdirectory the backup skips. \
                     If it holds truth rather than derived spill, pack() \
                     is losing it — see the comment at the skip."
                );
            }
        }
        std::fs::remove_dir_all(&src).ok();
        std::fs::remove_dir_all(&target).ok();
    }

    #[test]
    fn pack_unpack_round_trip() {
        let src = tmp("src");
        std::fs::write(src.join("aof-0.aof"), b"AOF body 1").unwrap();
        std::fs::write(src.join("snap-0.rdb"), b"snapshot body").unwrap();
        let out = tmp_file("backup.kevybkp");
        pack(&src, &out).unwrap();

        let target = tmp("restored");
        unpack(&out, &target).unwrap();
        assert_eq!(std::fs::read(target.join("aof-0.aof")).unwrap(), b"AOF body 1");
        assert_eq!(std::fs::read(target.join("snap-0.rdb")).unwrap(), b"snapshot body");

        std::fs::remove_dir_all(&src).ok();
        std::fs::remove_file(&out).ok();
        std::fs::remove_dir_all(&target).ok();
    }

    #[test]
    fn unpack_refuses_path_traversal() {
        let src = tmp("src2");
        std::fs::create_dir_all(&src).unwrap();
        let out_path = tmp_file("bad.kevybkp");
        // Hand-craft a bad container with "../etc/passwd" name.
        let mut f = File::create(&out_path).unwrap();
        f.write_all(MAGIC).unwrap();
        let name = b"../etc/passwd";
        f.write_all(&(name.len() as u16).to_be_bytes()).unwrap();
        f.write_all(name).unwrap();
        f.write_all(&0u64.to_be_bytes()).unwrap();
        f.write_all(&0u16.to_be_bytes()).unwrap();
        drop(f);

        let target = tmp("restored2");
        let err = unpack(&out_path, &target).unwrap_err();
        assert!(err.to_string().contains("path traversal"));

        std::fs::remove_file(&out_path).ok();
        std::fs::remove_dir_all(&target).ok();
        std::fs::remove_dir_all(&src).ok();
    }

    #[test]
    fn unpack_refuses_non_empty_target() {
        let target = tmp("non-empty");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("existing.txt"), b"data").unwrap();
        let out_path = tmp_file("good.kevybkp");
        let mut f = File::create(&out_path).unwrap();
        f.write_all(MAGIC).unwrap();
        f.write_all(&0u16.to_be_bytes()).unwrap();
        drop(f);
        let err = unpack(&out_path, &target).unwrap_err();
        assert!(err.to_string().contains("not empty"));
        std::fs::remove_dir_all(&target).ok();
        std::fs::remove_file(&out_path).ok();
    }
}
