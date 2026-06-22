//! Per-type payload writers for the binary snapshot format.
//!
//! Each `write_*_payload` helper emits one type variant's body
//! (already preceded by the record's op-code + ttl + key by
//! `crate::write_entry`). Split out of `crate::lib` to keep that
//! file under the 500-LOC house cap; the wire format itself is
//! unchanged.
//!
//! The snapshot wire format is encoding-agnostic for the inline /
//! heap pair — `Value::Set(Arc<KevySet>)` and
//! `Value::SmallSetInline(SmallSetData)` both serialise to the same
//! `OP_SET` `[len: u32 LE][bulk]*` byte stream, so a loader can
//! round-trip either encoding without knowing about the inline
//! variant. Same shape for Hash / List / ZSet.
//!
//! The matching `write_small_*_payload` helpers iterate the packed
//! inline buffer directly (no heap allocation, no detour through a
//! `KevyMap`/`VecDeque`/`BTreeSet`) — the natural shape for the
//! inline encoding.

use crate::write_bytes;
use std::io::{self, Write};

pub(crate) fn write_hash_payload<W: Write>(w: &mut W, h: &kevy_store::HashData) -> io::Result<()> {
    w.write_all(&(h.len() as u32).to_le_bytes())?;
    for (f, v) in h {
        write_bytes(w, f.as_slice())?;
        write_bytes(w, v)?;
    }
    Ok(())
}

pub(crate) fn write_list_payload<W: Write>(w: &mut W, l: &kevy_store::ListData) -> io::Result<()> {
    w.write_all(&(l.len() as u32).to_le_bytes())?;
    for item in l {
        write_bytes(w, item)?;
    }
    Ok(())
}

pub(crate) fn write_set_payload<W: Write>(
    w: &mut W,
    set: &kevy_store::SetData,
) -> io::Result<()> {
    w.write_all(&(set.len() as u32).to_le_bytes())?;
    for m in set {
        write_bytes(w, m.as_slice())?;
    }
    Ok(())
}

/// A.7 O5: inline-encoded set payload — same OP_SET wire shape as
/// [`write_set_payload`], sourced from the packed inline buffer.
pub(crate) fn write_small_set_payload<W: Write>(
    w: &mut W,
    s: &kevy_store::SmallSetData,
) -> io::Result<()> {
    w.write_all(&(s.len() as u32).to_le_bytes())?;
    for m in s.iter() {
        write_bytes(w, m)?;
    }
    Ok(())
}

/// A.8: inline hash payload — same OP_HASH wire shape as
/// [`write_hash_payload`].
pub(crate) fn write_small_hash_payload<W: Write>(
    w: &mut W,
    h: &kevy_store::SmallHashData,
) -> io::Result<()> {
    w.write_all(&(h.len() as u32).to_le_bytes())?;
    for (f, v) in h.iter() {
        write_bytes(w, f)?;
        write_bytes(w, v)?;
    }
    Ok(())
}

/// A.8: inline list payload — same OP_LIST wire shape as
/// [`write_list_payload`].
pub(crate) fn write_small_list_payload<W: Write>(
    w: &mut W,
    l: &kevy_store::SmallListData,
) -> io::Result<()> {
    w.write_all(&(l.len() as u32).to_le_bytes())?;
    for e in l.iter() {
        write_bytes(w, e)?;
    }
    Ok(())
}

/// A.8: inline zset payload — same OP_ZSET wire shape as
/// [`write_zset_payload`].
pub(crate) fn write_small_zset_payload<W: Write>(
    w: &mut W,
    z: &kevy_store::SmallZSetData,
) -> io::Result<()> {
    w.write_all(&(z.len() as u32).to_le_bytes())?;
    for (m, sc) in z.iter() {
        write_bytes(w, m)?;
        w.write_all(&sc.to_bits().to_le_bytes())?;
    }
    Ok(())
}

pub(crate) fn write_zset_payload<W: Write>(
    w: &mut W,
    z: &kevy_store::ZSetData,
) -> io::Result<()> {
    let entries: Vec<(&[u8], f64)> = z.ordered().collect();
    w.write_all(&(entries.len() as u32).to_le_bytes())?;
    for (m, score) in entries {
        write_bytes(w, m)?;
        w.write_all(&score.to_bits().to_le_bytes())?;
    }
    Ok(())
}

pub(crate) fn write_stream_payload<W: Write>(
    w: &mut W,
    s: &kevy_store::StreamData,
) -> io::Result<()> {
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
    crate::write_stream_groups(w, &s.export_groups())
}
