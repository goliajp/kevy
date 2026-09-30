//! The auxiliary record frame a snapshot and a rewritten log carry beside
//! the keyspace (see [`crate::SnapshotSource::aux_frame`]).
//!
//! In a snapshot it is one `OP_AUX` record, `[parts u32 LE][bytes]…`, after
//! `OP_EOF`. The version byte does not move: a reader that predates the
//! record returns at `OP_EOF` with every entry loaded and never reads it
//! (6.4.0 refused an unknown record before `OP_EOF` as a failed load).

use std::io::{self, Read, Write};

use kevy_store::Value;

use crate::snapshot_fmt::{OP_AUX, read_bytes, read_u32, write_bytes};
use crate::{Argv, SnapshotSource};

/// A [`SnapshotSource`] with an auxiliary frame beside another source's
/// keyspace: what a runtime hands the snapshot writer and the log rewriter
/// when it keeps state outside the store.
///
/// ```
/// use kevy_persist::{Argv, WithAux};
///
/// let store = kevy_store::Store::new();
/// let frame = Argv::from(vec![b"XINTERNAL.EXAMPLE".to_vec(), b"1".to_vec()]);
/// let mut image = Vec::new();
/// kevy_persist::write_snapshot_to(&WithAux::new(&store, Some(&frame)), &mut image)?;
/// let mut back = kevy_store::Store::new();
/// let aux = kevy_persist::load_snapshot_with_aux(&mut back, image.as_slice(), |_| true)?;
/// assert_eq!(aux.as_ref().and_then(|a| a.get(1)), Some(&b"1"[..]));
/// # Ok::<(), std::io::Error>(())
/// ```
#[derive(Debug, Clone, Copy)]
pub struct WithAux<'a, S> {
    src: &'a S,
    aux: Option<&'a Argv>,
}

impl<'a, S: SnapshotSource> WithAux<'a, S> {
    /// `src`'s keyspace with `aux` beside it (`None` = nothing beside it).
    ///
    /// ```
    /// use kevy_persist::{SnapshotSource, WithAux};
    ///
    /// let store = kevy_store::Store::new();
    /// assert!(WithAux::new(&store, None).aux_frame().is_none());
    /// ```
    pub fn new(src: &'a S, aux: Option<&'a Argv>) -> Self {
        Self { src, aux }
    }
}

impl<S: SnapshotSource> SnapshotSource for WithAux<'_, S> {
    fn for_each_entry(&self, f: impl FnMut(&[u8], &Value, Option<u64>)) {
        self.src.for_each_entry(f);
    }
    fn for_each_hash_ttl(&self, f: impl FnMut(&[u8], &[u8], u64)) {
        self.src.for_each_hash_ttl(f);
    }
    fn row_seg_files(&self) -> Vec<(u32, String)> {
        self.src.row_seg_files()
    }
    fn aux_frame(&self) -> Option<Argv> {
        self.aux.cloned()
    }
}

/// Write `frame` as the snapshot's `OP_AUX` record.
pub(crate) fn write_aux<W: Write>(w: &mut W, frame: &Argv) -> io::Result<()> {
    w.write_all(&[OP_AUX])?;
    let n = u32::try_from(frame.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "aux frame too long"))?;
    w.write_all(&n.to_le_bytes())?;
    for i in 0..frame.len() {
        write_bytes(w, &frame[i])?;
    }
    Ok(())
}

/// What follows `OP_EOF`: the aux frame, or nothing. Bytes after it that
/// are not an aux record are left unread, as a reader from before the
/// record leaves them.
pub(crate) fn read_trailer<R: Read>(r: &mut R) -> io::Result<Option<Argv>> {
    let mut op = [0u8; 1];
    match r.read_exact(&mut op) {
        Ok(()) if op[0] == OP_AUX => read_aux(r).map(Some),
        Ok(()) => Ok(None),
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => Ok(None),
        Err(e) => Err(e),
    }
}

/// Read an `OP_AUX` record's body (the opcode already consumed).
fn read_aux<R: Read>(r: &mut R) -> io::Result<Argv> {
    let n = read_u32(r)? as usize;
    let mut parts = Vec::with_capacity(n.min(16));
    for _ in 0..n {
        parts.push(read_bytes(r)?);
    }
    Ok(Argv::from(parts))
}

#[cfg(test)]
mod tests {
    use super::*;
    use kevy_store::{SetCondition, Store};

    fn frame() -> Argv {
        Argv::from(vec![b"XINTERNAL.EXAMPLE".to_vec(), b"7".to_vec(), Vec::new(), vec![0, 255]])
    }

    fn store() -> Store {
        let mut s = Store::new();
        s.set(b"k", b"v".to_vec(), None, SetCondition::Always);
        s.hset(b"h", &[(b"f", b"v")]).unwrap();
        s.hexpire_at(b"h", &[b"f"], kevy_store::now_unix_ms() + 60_000, Default::default())
            .unwrap();
        s
    }

    /// The frame comes back whole from a snapshot, after every entry and
    /// field deadline, and loading it leaves the keyspace as it was.
    #[test]
    fn a_snapshot_carries_the_frame_after_the_keyspace() {
        let (s, f) = (store(), frame());
        let mut image = Vec::new();
        crate::write_snapshot_to(&WithAux::new(&s, Some(&f)), &mut image).unwrap();
        let mut bare = Vec::new();
        crate::write_snapshot_to(&s, &mut bare).unwrap();
        // the image a reader from before the frame reads to its end
        assert_eq!(image[..bare.len()], bare[..]);
        assert_eq!(bare[bare.len() - 1], crate::snapshot_fmt::OP_EOF);
        let mut back = Store::new();
        let aux = crate::load_snapshot_with_aux(&mut back, image.as_slice(), |_| true).unwrap();
        assert_eq!(aux, Some(f));
        assert_eq!(back.dbsize(), 2);
        // the loaders that do not ask for it read the same image
        let mut plain = Store::new();
        crate::load_snapshot_from(&mut plain, image.as_slice()).unwrap();
        assert_eq!(plain.dbsize(), 2);
    }

    /// A snapshot without a frame is byte for byte what it was before
    /// frames existed.
    #[test]
    fn no_frame_changes_nothing() {
        let s = store();
        let (mut plain, mut wrapped) = (Vec::new(), Vec::new());
        crate::write_snapshot_to(&s, &mut plain).unwrap();
        crate::write_snapshot_to(&WithAux::new(&s, None), &mut wrapped).unwrap();
        assert_eq!(plain, wrapped);
    }

    /// A rewritten log ends with the frame, so replaying it replays the
    /// frame after the keyspace.
    #[test]
    fn a_rewritten_log_ends_with_the_frame() {
        let (s, f) = (store(), frame());
        let dir = kevy_tmpdir::unique_dir("aux-rewrite");
        let path = dir.join("aof-0.aof");
        crate::dump_aof(&path, &WithAux::new(&s, Some(&f))).unwrap();
        let mut frames = Vec::new();
        crate::replay_aof_quiet(&path, Default::default(), |a| frames.push(a)).unwrap();
        assert_eq!(frames.last(), Some(&f));
        let (image, _) =
            crate::dump_store_to_buf(&WithAux::new(&s, Some(&f)), crate::AofFormat::V2);
        let buf_path = dir.join("aof-1.aof");
        std::fs::write(&buf_path, image).unwrap();
        let mut from_buf = Vec::new();
        crate::replay_aof_quiet(&buf_path, Default::default(), |a| from_buf.push(a)).unwrap();
        assert_eq!(from_buf, frames);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Wherever the sink fills up, the record's write fails rather than
    /// leaving a short record behind a success.
    #[test]
    fn a_frame_that_does_not_fit_its_sink_fails_to_write() {
        let f = frame();
        let mut whole = Vec::new();
        write_aux(&mut whole, &f).unwrap();
        // no room for the opcode, for the part count, for the first part
        for room in [0, 1, 5] {
            let mut buf = vec![0u8; room];
            let err = write_aux(&mut buf.as_mut_slice(), &f).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::WriteZero, "room {room}");
            assert_eq!(buf[..], whole[..room]);
        }
        // a snapshot whose frame outgrows its buffer meets the full sink there
        let big = Argv::from(vec![vec![b'x'; crate::snapshot_fmt::SNAPSHOT_BUF_CAP + 1]]);
        let (mut sink, empty) = ([0u8; 64], Store::new());
        let src = WithAux::new(&empty, Some(&big));
        let err = crate::write_snapshot_to(&src, sink.as_mut_slice()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::WriteZero);
    }

    /// After `OP_EOF`: another opcode is not a frame, a cut frame is an
    /// error, and so is a read that fails for any reason but the end.
    #[test]
    fn what_follows_the_end_is_a_frame_nothing_or_an_error() {
        assert_eq!(read_trailer(&mut &[0x42u8][..]).unwrap(), None);
        assert_eq!(read_trailer(&mut &[][..]).unwrap(), None);
        let cut_count = [OP_AUX, 1, 0];
        let err = read_trailer(&mut &cut_count[..]).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
        let cut_part = [OP_AUX, 1, 0, 0, 0, 9, 0, 0, 0, b'x'];
        let err = read_trailer(&mut &cut_part[..]).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
        // reading a directory fails with something other than the end
        let dir = kevy_tmpdir::TmpDir::new("aux-trailer-dir");
        let mut not_a_file = std::fs::File::open(dir.path()).unwrap();
        let err = read_trailer(&mut not_a_file).unwrap_err();
        assert_ne!(err.kind(), io::ErrorKind::UnexpectedEof, "{err}");
    }
}
