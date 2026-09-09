//! The segment manifest — the append-only ledger that makes a segment
//! REAL. A sealed `.seg` file not recorded here is garbage by
//! contract (a crash mid-build leaves exactly that), and the startup
//! sweep deletes it. Records carry opaque caller metadata (table,
//! bucket — this crate does not know what they mean) plus the segment
//! summary needed to serve a directory without opening every file.
//!
//! On-disk: `[u32-LE len][u32-LE crc32c][payload]` per record — the
//! AOF v2 envelope shape. A torn tail (crash mid-append) is tolerated
//! by truncation at the last whole record; a corrupt record BEFORE
//! the tail is a named refusal — that is bit rot, not a crash.
//! Compaction rewrites the live set to a temp file and renames over
//! (the snapshot-swap precedent); `fsync` on every append is the
//! caller's durability point (step 2 of the eviction order).

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::SegError;

/// One live segment as the manifest knows it.
#[derive(Debug, Clone, PartialEq, Eq)]
/// # Examples
///
/// ```
/// use kevy_seg::{Manifest, ManifestEntry};
/// # let dir = std::env::temp_dir().join(format!("kevy-man-doc-{}-{}", std::process::id(), line!()));
/// # std::fs::create_dir_all(&dir).unwrap();
/// let mut m = Manifest::open(&dir).unwrap();
/// let e = ManifestEntry {
///     file: "s1.seg".into(),
///     meta: Vec::new(),
///     min_key: b"a".to_vec(),
///     max_key: b"z".to_vec(),
///     records: 1,
/// };
/// m.add(e).unwrap();
/// // min/max are mirrored from the footer so a reader can skip a
/// // segment without opening it.
/// let first = m.live().next().unwrap();
/// assert_eq!(first.file, "s1.seg");
/// assert_eq!(first.max_key, b"z".to_vec());
/// # std::fs::remove_dir_all(&dir).ok();
/// ```
pub struct ManifestEntry {
    /// Segment file name (relative to the manifest's directory).
    pub file: String,
    /// Caller-opaque metadata (table id, bucket, …).
    pub meta: Vec<u8>,
    /// Smallest key in the segment, mirrored from its footer so a
    /// directory can be served without opening the file.
    pub min_key: Vec<u8>,
    /// Largest key in the segment. With `min_key` this is the range a
    /// reader tests before deciding the segment is worth opening.
    pub max_key: Vec<u8>,
    /// Record count, mirrored from the footer. Includes tombstones, so
    /// it bounds a scan rather than predicting what it yields.
    pub records: u64,
}

/// The append-only ledger. One per segment directory.
#[derive(Debug)]
pub struct Manifest {
    path: PathBuf,
    f: File,
    /// Live entries by file name (a DROP removes; ADD of a name
    /// already live is a refusal — segments are immutable, a name
    /// never means two things).
    live: BTreeMap<String, ManifestEntry>,
}

const MANIFEST: &str = "segs.manifest";
/// The largest record this writer will ever produce, and therefore the
/// largest a reader may believe.
///
/// A record is a file name, a small meta blob, two segment keys and a
/// count; keys are bounded by the page a cell must fit in, so this is
/// generous by a wide margin. It exists because the length field is the
/// one part of a record that **nothing checks** — the CRC covers the
/// body, and the length sits outside it. See `open` for what that cost.
const MAX_RECORD_BYTES: usize = 64 * 1024;

/// The most that can legitimately be discarded as a torn tail: one
/// envelope. `append` is a single `write_all` followed by `sync_all`, so
/// at most one record can ever be partly on disk.
const MAX_TORN_TAIL: usize = 8 + MAX_RECORD_BYTES;

const OP_ADD: u8 = 1;
const OP_DROP: u8 = 2;

impl Manifest {
    /// Open (or start) the ledger in `dir`, replaying it to the live
    /// set. A torn tail is truncated; corruption before the tail is a
    /// named refusal.
    /// # Examples
    ///
    /// ```
    /// use kevy_seg::{Manifest, ManifestEntry};
    /// # let dir = std::env::temp_dir().join(format!("kevy-man-doc-{}-{}", std::process::id(), line!()));
    /// # std::fs::create_dir_all(&dir).unwrap();
    /// // A directory with no manifest opens empty rather than failing.
    /// let m = Manifest::open(&dir).unwrap();
    /// assert_eq!(m.live().count(), 0);
    /// # std::fs::remove_dir_all(&dir).ok();
    /// ```
    pub fn open(dir: &Path) -> Result<Self, SegError> {
        let path = dir.join(MANIFEST);
        let mut live = BTreeMap::new();
        let good = if path.exists() { replay(&std::fs::read(&path)?, &mut live)? } else { 0 };
        let f = OpenOptions::new().create(true).append(true).open(&path)?;
        if path.metadata()?.len() > good {
            // Drop the torn tail so the next append starts clean.
            f.set_len(good)?;
        }
        Ok(Self { path, f, live })
    }

    /// Record a sealed segment. Fsyncs before returning — this IS the
    /// durability point that makes the segment real.
    /// # Examples
    ///
    /// ```
    /// use kevy_seg::{Manifest, ManifestEntry};
    /// # let dir = std::env::temp_dir().join(format!("kevy-man-doc-{}-{}", std::process::id(), line!()));
    /// # std::fs::create_dir_all(&dir).unwrap();
    /// let mut m = Manifest::open(&dir).unwrap();
    /// let e = ManifestEntry {
    ///     file: "s1.seg".into(),
    ///     meta: Vec::new(),
    ///     min_key: b"a".to_vec(),
    ///     max_key: b"z".to_vec(),
    ///     records: 1,
    /// };
    /// m.add(e).unwrap();
    /// // Durable at once: a fresh open sees it.
    /// assert_eq!(Manifest::open(&dir).unwrap().live().count(), 1);
    /// # std::fs::remove_dir_all(&dir).ok();
    /// ```
    pub fn add(&mut self, e: ManifestEntry) -> Result<(), SegError> {
        if self.live.contains_key(&e.file) {
            return Err(SegError::Corrupt("segment name already live"));
        }
        self.append(OP_ADD, &encode_entry(&e))?;
        self.live.insert(e.file.clone(), e);
        Ok(())
    }

    /// Record a segment's retirement (compacted away / emptied by
    /// tombstones). The file itself is unlinked by the caller AFTER
    /// this returns — the ledger must never point at nothing.
    /// # Examples
    ///
    /// ```
    /// use kevy_seg::{Manifest, ManifestEntry};
    /// # let dir = std::env::temp_dir().join(format!("kevy-man-doc-{}-{}", std::process::id(), line!()));
    /// # std::fs::create_dir_all(&dir).unwrap();
    /// let mut m = Manifest::open(&dir).unwrap();
    /// let e = ManifestEntry {
    ///     file: "s1.seg".into(),
    ///     meta: Vec::new(),
    ///     min_key: b"a".to_vec(),
    ///     max_key: b"z".to_vec(),
    ///     records: 1,
    /// };
    /// m.add(e).unwrap();
    /// m.drop_seg("s1.seg").unwrap();
    /// // Gone from the live set — the FILE is removed later, by `sweep`.
    /// assert_eq!(m.live().count(), 0);
    /// # std::fs::remove_dir_all(&dir).ok();
    /// ```
    pub fn drop_seg(&mut self, file: &str) -> Result<(), SegError> {
        if !self.live.contains_key(file) {
            return Err(SegError::Corrupt("dropping a segment the ledger does not hold"));
        }
        self.append(OP_DROP, file.as_bytes())?;
        self.live.remove(file);
        Ok(())
    }

    /// The live set, name-ordered.
    /// # Examples
    ///
    /// ```
    /// use kevy_seg::{Manifest, ManifestEntry};
    /// # let dir = std::env::temp_dir().join(format!("kevy-man-doc-{}-{}", std::process::id(), line!()));
    /// # std::fs::create_dir_all(&dir).unwrap();
    /// let mut m = Manifest::open(&dir).unwrap();
    /// let e = ManifestEntry {
    ///     file: "s1.seg".into(),
    ///     meta: Vec::new(),
    ///     min_key: b"a".to_vec(),
    ///     max_key: b"z".to_vec(),
    ///     records: 1,
    /// };
    /// m.add(e).unwrap();
    /// assert_eq!(m.live().map(|e| e.file.as_str()).collect::<Vec<_>>(), vec!["s1.seg"]);
    /// # std::fs::remove_dir_all(&dir).ok();
    /// ```
    pub fn live(&self) -> impl Iterator<Item = &ManifestEntry> {
        self.live.values()
    }

    /// Rewrite the ledger as its live set (temp + rename + fsync).
    /// Call when the dead-record fraction is worth the IO.
    pub fn compact(&mut self) -> Result<(), SegError> {
        let tmp = self.path.with_extension("manifest.rewrite");
        let mut w = File::create(&tmp)?;
        for e in self.live.values() {
            w.write_all(&envelope(OP_ADD, &encode_entry(e)))?;
        }
        w.sync_all()?;
        std::fs::rename(&tmp, &self.path)?;
        self.f = OpenOptions::new().append(true).open(&self.path)?;
        Ok(())
    }

    /// Delete every `.seg` file in `dir` the ledger does not hold —
    /// the startup sweep that turns crash-mid-build leftovers back
    /// into free disk. Returns the swept names.
    /// # Examples
    ///
    /// ```
    /// use kevy_seg::{Manifest, ManifestEntry};
    /// # let dir = std::env::temp_dir().join(format!("kevy-man-doc-{}-{}", std::process::id(), line!()));
    /// # std::fs::create_dir_all(&dir).unwrap();
    /// let mut m = Manifest::open(&dir).unwrap();
    /// let e = ManifestEntry {
    ///     file: "s1.seg".into(),
    ///     meta: Vec::new(),
    ///     min_key: b"a".to_vec(),
    ///     max_key: b"z".to_vec(),
    ///     records: 1,
    /// };
    /// m.add(e).unwrap();
    /// std::fs::write(dir.join("s1.seg"), b"x").unwrap();
    /// std::fs::write(dir.join("orphan.seg"), b"x").unwrap();
    ///
    /// // Only files the manifest does not claim are swept.
    /// let gone = m.sweep(&dir).unwrap();
    /// assert_eq!(gone, vec!["orphan.seg".to_string()]);
    /// assert!(dir.join("s1.seg").exists());
    /// # std::fs::remove_dir_all(&dir).ok();
    /// ```
    pub fn sweep(&self, dir: &Path) -> Result<Vec<String>, SegError> {
        let mut swept = Vec::new();
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.ends_with(".seg") && !self.live.contains_key(&name) {
                std::fs::remove_file(entry.path())?;
                swept.push(name);
            }
        }
        Ok(swept)
    }

    fn append(&mut self, op: u8, payload: &[u8]) -> Result<(), SegError> {
        // The reader trusts that no record exceeds this, and that trust
        // is what lets it tell a torn tail from a corrupt length. Refuse
        // here rather than write something it would later have to call
        // corruption.
        if 1 + payload.len() > MAX_RECORD_BYTES {
            return Err(SegError::Corrupt("manifest record too large"));
        }
        self.f.write_all(&envelope(op, payload))?;
        self.f.sync_all()?;
        Ok(())
    }
}

/// `[len][crc][op ‖ payload]` — crc covers op+payload.
fn envelope(op: u8, payload: &[u8]) -> Vec<u8> {
    let mut body = Vec::with_capacity(1 + payload.len());
    body.push(op);
    body.extend_from_slice(payload);
    let mut out = Vec::with_capacity(8 + body.len());
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&kevy_sys::checksum::crc32c(&body).to_le_bytes());
    out.extend_from_slice(&body);
    out
}

/// Decode the record at `o`; `None` = incomplete or crc-mismatched
/// (the caller distinguishes torn tail from mid-file rot by position).
fn read_record(bytes: &[u8], o: usize) -> Option<(Vec<u8>, usize)> {
    let len = u32::from_le_bytes(bytes.get(o..o + 4)?.try_into().ok()?) as usize;
    let want = u32::from_le_bytes(bytes.get(o + 4..o + 8)?.try_into().ok()?);
    let body = bytes.get(o + 8..o + 8 + len)?;
    (kevy_sys::checksum::crc32c(body) == want).then(|| (body.to_vec(), o + 8 + len))
}

/// Replay every whole record, returning the offset of the last good one.
///
/// The three refusals here are what separate a torn tail — a crash
/// part-way through one `append` — from a damaged length field. That
/// distinction used to not exist: the length is the only part of a
/// record nothing covers, since the CRC is over the body, so a bit
/// flipped there made the record undecodable, and an undecodable record
/// was taken for a tail and everything after it truncated away.
///
/// Measured: one bit flipped in the FIRST record's length took a
/// five-entry, 220-byte ledger to zero entries and zero bytes with
/// `open` returning `Ok` — and `sweep` then deletes every segment file
/// the ledger no longer names. Over every bit of every length field,
/// 96 flips silently dropped entries; now none does, except in the last
/// record, where nothing follows to contradict the claim and the loss is
/// the one record a real torn tail would have cost anyway.
fn replay(bytes: &[u8], live: &mut BTreeMap<String, ManifestEntry>) -> Result<u64, SegError> {
    let mut good = 0u64;
    let mut o = 0usize;
    while o < bytes.len() {
        match read_record(bytes, o) {
            Some((rec, next)) => {
                apply(live, &rec)?;
                good = next as u64;
                o = next;
            }
            None if whole_records_end(bytes, o) => {
                return Err(SegError::Corrupt("manifest record crc"));
            }
            // A length no `append` could have written.
            None if !plausible_len(bytes, o) => {
                return Err(SegError::Corrupt("manifest length field"));
            }
            // More left than one envelope can be; `append` writes one
            // record and fsyncs, so at most one is ever partly on disk.
            None if bytes.len() - o > MAX_TORN_TAIL => {
                return Err(SegError::Corrupt("manifest length field"));
            }
            // Something after this decodes, so this is not the end.
            None if valid_record_follows(bytes, o) => {
                return Err(SegError::Corrupt("manifest length field"));
            }
            None => break, // torn tail: crash mid-append
        }
    }
    Ok(good)
}

/// Whether a decodable record begins anywhere after `o`.
///
/// A torn tail is the last thing in the file by construction — the
/// writer appends one record and fsyncs, so nothing follows a partial
/// one. If something after `o` decodes and passes its CRC, then what is
/// at `o` is not a tail, and treating it as one throws that record away.
///
/// This is what makes the recovery rule decidable rather than a guess.
/// The length bound above catches a wildly wrong length; this catches a
/// plausible one, which is the case that would otherwise silently
/// discard every record after the damaged one.
///
/// Cost is a CRC per candidate offset over the bytes that would be
/// discarded, paid once at open and only when a record has already
/// failed to decode — never on a healthy ledger.
fn valid_record_follows(bytes: &[u8], o: usize) -> bool {
    (o + 1..bytes.len().saturating_sub(8)).any(|p| read_record(bytes, p).is_some())
}

/// Whether the length field at `o` could have been written by
/// [`Manifest::append`].
///
/// This is the check that separates a torn tail from a flipped bit. The
/// length is the only part of a record nothing covers — the CRC is over
/// the body — so a bit flipped there makes the record undecodable, and
/// an undecodable record at the end looks exactly like a crash
/// mid-append. Taking it for one truncates everything after it.
///
/// A length beyond what `append` will write is not a tail under any
/// circumstances, so it is the discriminator. Note the direction: a
/// flipped bit that makes the length SMALLER is already caught, because
/// the body then fails its CRC with a whole record's bytes present.
fn plausible_len(bytes: &[u8], o: usize) -> bool {
    // Fewer than four bytes left is a torn tail by definition: the
    // writer was interrupted before it finished the length field. Saying
    // "not plausible" here would turn the most ordinary crash into a
    // refusal to open — which an exhaustive check of this caught.
    let Some(b) = bytes.get(o..o + 4) else { return true };
    let len = u32::from_le_bytes(b.try_into().expect("bytes.get(o..o + 4) returned Some")) as usize;
    len <= MAX_RECORD_BYTES
}

/// Whether a full record's worth of bytes exists at `o` (so a decode
/// failure there is rot, not a torn tail).
fn whole_records_end(bytes: &[u8], o: usize) -> bool {
    let Some(len_bytes) = bytes.get(o..o + 4) else { return false };
    let len = u32::from_le_bytes(len_bytes.try_into().expect("bytes.get(o..o + 4) returned Some"))
        as usize;
    bytes.len() >= o + 8 + len
}

fn apply(live: &mut BTreeMap<String, ManifestEntry>, rec: &[u8]) -> Result<(), SegError> {
    match rec.first() {
        Some(&OP_ADD) => {
            let e = decode_entry(&rec[1..]).ok_or(SegError::Corrupt("manifest ADD shape"))?;
            if live.insert(e.file.clone(), e).is_some() {
                return Err(SegError::Corrupt("duplicate ADD in manifest"));
            }
            Ok(())
        }
        Some(&OP_DROP) => {
            let name = String::from_utf8_lossy(&rec[1..]).into_owned();
            live.remove(&name).map(|_| ()).ok_or(SegError::Corrupt("DROP of an unknown segment"))
        }
        _ => Err(SegError::Corrupt("unknown manifest op")),
    }
}

fn encode_entry(e: &ManifestEntry) -> Vec<u8> {
    let mut b = Vec::new();
    for part in [e.file.as_bytes(), &e.meta, &e.min_key, &e.max_key] {
        b.extend_from_slice(&(part.len() as u32).to_le_bytes());
        b.extend_from_slice(part);
    }
    b.extend_from_slice(&e.records.to_le_bytes());
    b
}

fn take<'a>(b: &'a [u8], o: &mut usize, n: usize) -> Option<&'a [u8]> {
    let s = b.get(*o..*o + n)?;
    *o += n;
    Some(s)
}

fn part(b: &[u8], o: &mut usize) -> Option<Vec<u8>> {
    let len = u32::from_le_bytes(take(b, o, 4)?.try_into().ok()?) as usize;
    Some(take(b, o, len)?.to_vec())
}

fn decode_entry(b: &[u8]) -> Option<ManifestEntry> {
    let mut o = 0usize;
    let file = String::from_utf8(part(b, &mut o)?).ok()?;
    let meta = part(b, &mut o)?;
    let min_key = part(b, &mut o)?;
    let max_key = part(b, &mut o)?;
    let records = u64::from_le_bytes(take(b, &mut o, 8)?.try_into().ok()?);
    (o == b.len()).then_some(ManifestEntry { file, meta, min_key, max_key, records })
}
