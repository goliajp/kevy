//! Which snapshot a shard's log continues, and so what a restart restores.
//!
//! A shard's log is one of two things. After a snapshot commits, it holds
//! only the writes made after that snapshot froze, and a restore loads the
//! snapshot and replays the log over it. After a rewrite, it is a complete
//! image of the keyspace, and loading any snapshot under it applies every
//! write twice (a list appended to twice, a counter incremented twice).
//!
//! The log says which it is in its first record, the `LOG_BASE` frame:
//! the id of the snapshot it continues, or 0 for a log that stands alone.
//! Every snapshot file ends with its own id, so a restore pairs the two by
//! value instead of by which files happen to exist. A log without the frame
//! was written before it existed and keeps the old rule: the snapshot, then
//! the log.
//!
//! Committing a snapshot changes two files. The previous snapshot is kept
//! as `<snapshot>.prev` until the log that continues the new one is in
//! place, so a crash between the two renames leaves a pair a restart can
//! still match: [`settle_snapshot`] keeps whichever snapshot the log names.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use kevy_resp::{Argv, ArgvView};

/// The log-base frame's verb. The leading NUL keeps it out of reach of any
/// verb a client can send, like the transaction markers.
pub(crate) const LOG_BASE: &[u8] = b"\0KEVYLOGBASE";

/// The id a log that stands alone names: a rewrite image.
pub(crate) const STANDS_ALONE: u64 = 0;

/// The snapshot id footer: this tag, then the id as u64 LE, as the file's
/// last bytes. Readers that stop at the end-of-entries record never see it.
const SNAPSHOT_ID_TAG: &[u8; 8] = b"KEVYSNID";
const FOOTER: usize = SNAPSHOT_ID_TAG.len() + 8;

/// What a log's first record says about the snapshot under it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LogHead {
    /// No log file.
    Missing,
    /// A log from before the frame existed (or one never written past
    /// its magic).
    Legacy,
    /// A complete image: restores from itself alone.
    StandsAlone,
    /// The writes after the snapshot with this id.
    After(u64),
}

/// The `LOG_BASE` frame naming `snapshot_id`.
pub(crate) fn base_frame(snapshot_id: u64) -> Argv {
    let mut frame = Argv::with_capacity(2, LOG_BASE.len() + 8);
    frame.push(LOG_BASE);
    frame.push(&snapshot_id.to_le_bytes());
    frame
}

/// Write the frame that opens a rewrite image: a log that stands alone.
pub(crate) fn write_image_base<W: Write>(w: &mut W, scratch: &mut Vec<u8>) -> io::Result<()> {
    crate::record::write_record_multibulk(w, &base_frame(STANDS_ALONE), scratch)
}

/// The snapshot id `args` names, if it is a `LOG_BASE` frame.
pub(crate) fn base_of<A: ArgvView + ?Sized>(args: &A) -> Option<u64> {
    if args.len() != 2 || args.get(0) != Some(LOG_BASE) {
        return None;
    }
    args.get(1).and_then(|id| <[u8; 8]>::try_from(id).ok()).map(u64::from_le_bytes)
}

/// Whether `args` is the record that opens a log and names the snapshot it
/// continues. A replay skips it and does not count it as a write.
///
/// ```
/// let write: kevy_persist::Argv = vec![b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()].into();
/// assert!(!kevy_persist::is_log_base(&write));
/// ```
pub fn is_log_base<A: ArgvView + ?Sized>(args: &A) -> bool {
    base_of(args).is_some()
}

/// Read the first record of the log at `path`.
pub(crate) fn read_log_head(path: &Path) -> io::Result<LogHead> {
    let mut file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(LogHead::Missing),
        Err(e) => return Err(e),
    };
    // the frame is well under this; a longer first record is not the frame
    let mut head = Vec::with_capacity(128);
    Read::take(&mut file, 128).read_to_end(&mut head)?;
    let magic = crate::record::AOF2_MAGIC;
    if !head.starts_with(magic) {
        return Ok(LogHead::Legacy);
    }
    let crate::RecordStep::Ok { payload, .. } = crate::next_record(&head, magic.len()) else {
        return Ok(LogHead::Legacy);
    };
    let mut args = Argv::default();
    if !matches!(kevy_resp::parse_command_into(payload, &mut args), Ok(Some(_))) {
        return Ok(LogHead::Legacy);
    }
    Ok(match base_of(&args) {
        Some(STANDS_ALONE) => LogHead::StandsAlone,
        Some(id) => LogHead::After(id),
        None => LogHead::Legacy,
    })
}

/// A fresh nonzero snapshot id. Unique enough that a log and a snapshot
/// from different commits never share one.
pub(crate) fn fresh_snapshot_id() -> u64 {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let place = &SEQ as *const AtomicU64 as u64;
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let mut z = kevy_store::now_unix_ms()
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .wrapping_add(seq.wrapping_mul(0xBF58_476D_1CE4_E5B9))
        ^ place.rotate_left(29);
    // splitmix64 finalizer
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;
    z.max(1)
}

/// Append a fresh id footer to a snapshot being written; returns the id.
pub(crate) fn write_snapshot_id<W: Write>(w: &mut W) -> io::Result<u64> {
    let id = fresh_snapshot_id();
    let mut footer = [0u8; FOOTER];
    footer[..SNAPSHOT_ID_TAG.len()].copy_from_slice(SNAPSHOT_ID_TAG);
    footer[SNAPSHOT_ID_TAG.len()..].copy_from_slice(&id.to_le_bytes());
    w.write_all(&footer)?;
    Ok(id)
}

/// The id the snapshot at `path` ends with; `None` for a missing file or
/// one written before snapshots carried an id.
pub(crate) fn snapshot_id(path: &Path) -> io::Result<Option<u64>> {
    let mut file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    if file.metadata()?.len() < FOOTER as u64 {
        return Ok(None);
    }
    file.seek(SeekFrom::End(-(FOOTER as i64)))?;
    let mut footer = [0u8; FOOTER];
    file.read_exact(&mut footer)?;
    let (tag, id) = footer.split_at(SNAPSHOT_ID_TAG.len());
    if tag != SNAPSHOT_ID_TAG {
        return Ok(None);
    }
    Ok(Some(u64::from_le_bytes(id.try_into().expect("the footer's second half is 8 bytes"))))
}

/// Where a snapshot commit keeps the snapshot it replaces until the log
/// that continues the new one is in place.
pub(crate) fn prev_path(snapshot: &Path) -> PathBuf {
    let mut s = snapshot.as_os_str().to_owned();
    s.push(".prev");
    s.into()
}

/// Settle a shard's snapshot against its log and answer whether a restore
/// loads `snapshot` before replaying `log` (`None` = the log is not
/// replayed).
///
/// A snapshot commit that stopped between its two renames is finished or
/// undone first: the snapshot the log continues is kept. Then a log that
/// is a complete image restores alone, a log that continues a snapshot
/// restores over exactly that snapshot, and a log from before logs named
/// their snapshot restores over whatever snapshot is there.
///
/// # Errors
///
/// A log that continues a snapshot that is not there, or not that one, is
/// refused by name: restoring it alone or over another snapshot would each
/// serve a keyspace that never existed.
///
/// ```
/// use kevy_persist::{Aof, Fsync, save_snapshot, settle_snapshot};
/// use kevy_store::{SetCondition, Store};
///
/// let dir = kevy_tmpdir::unique_dir("settle-doc");
/// let (snap, log) = (dir.join("dump-0.rdb"), dir.join("aof-0.aof"));
/// let mut store = Store::new();
/// store.set(b"k", b"v".to_vec(), None, SetCondition::Always);
/// save_snapshot(&store, &snap)?;
/// // a rewrite makes the log a complete image: the snapshot is not loaded under it
/// Aof::open(&log, Fsync::No)?.rewrite_from(&store)?;
/// assert!(!settle_snapshot(&snap, Some(&log))?);
/// // without a log, the snapshot is all there is
/// assert!(settle_snapshot(&snap, None)?);
/// # std::fs::remove_dir_all(&dir)?;
/// # Ok::<(), std::io::Error>(())
/// ```
pub fn settle_snapshot(snapshot: &Path, log: Option<&Path>) -> io::Result<bool> {
    let head = match log {
        Some(path) => read_log_head(path)?,
        None => LogHead::Missing,
    };
    let prev = prev_path(snapshot);
    if prev.exists() {
        let committed = match head {
            LogHead::After(id) => snapshot_id(snapshot)? == Some(id),
            LogHead::Missing => snapshot.exists(),
            LogHead::Legacy | LogHead::StandsAlone => false,
        };
        if committed {
            std::fs::remove_file(&prev)?;
        } else {
            std::fs::rename(&prev, snapshot)?;
        }
    }
    match head {
        LogHead::StandsAlone => Ok(false),
        LogHead::After(id) => match snapshot_id(snapshot)? {
            Some(found) if found == id => Ok(true),
            found => Err(unpaired(snapshot, log, id, found)),
        },
        LogHead::Legacy | LogHead::Missing => Ok(snapshot.exists()),
    }
}

fn unpaired(snapshot: &Path, log: Option<&Path>, id: u64, found: Option<u64>) -> io::Error {
    let log = log.map_or_else(String::new, |p| p.display().to_string());
    let found = match found {
        Some(other) => format!("is snapshot {other:016x}"),
        None if snapshot.exists() => String::from("carries no snapshot id"),
        None => String::from("is missing"),
    };
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!(
            "{log} holds the writes after snapshot {id:016x}, but {} {found}; \
             restore that snapshot, or move the log aside to start from the snapshot alone",
            snapshot.display()
        ),
    )
}
