//! Binary snapshot wire format: magic/version/opcode constants plus the
//! little-endian read/write primitives shared by the writer
//! (`crate::snapshot_write`) and the loader (`crate::snapshot_read`).
//! Split out of `lib.rs` to keep it under the 500-LOC house cap.

use std::io::{self, Read, Write};

/// File magic + format version. Bump `VERSION` on any layout change.
///
/// v2 stored each entry's TTL as **remaining millis** (relative), so a load
/// re-anchored the deadline to load-time — a restart reset every key to a
/// fresh full TTL (INC-2026-06-09). v3 stores the **absolute** Unix-ms
/// deadline, so a load reconstructs the original instant. v4 appends a
/// consumer-group section to each `OP_STREAM` payload (groups + consumers
/// plus PEL) — before that, SAVE/reshard silently dropped group state. The
/// loader still accepts v2 (relative TTL) and v3 (no group section).
pub(crate) const MAGIC: &[u8; 8] = b"KEVYSNAP";
pub(crate) const VERSION: u8 = 4;
/// v2.3: version 5 carries a 16-byte feed cursor (`gen u64 LE` +
/// `offset u64 LE`) right after the version byte — the snapshot half
/// of the recovery-point contract (docs/cdc.md): snapshot S + feed
/// frames from S's cursor = exact restore. Writers emit v5 only when
/// a cursor is supplied; cursor-less writes stay at v4 so every
/// existing path is byte-identical.
pub(crate) const VERSION_FEED_CURSOR: u8 = 5;
/// v2.4: version 6 additionally carries `OP_HFTTL` hash field-TTL
/// records after the entry stream. Written only when field TTLs
/// exist; the header still carries the (possibly zero) feed cursor.
pub(crate) const VERSION_HASH_TTL: u8 = 6;
pub(crate) const VERSION_RELATIVE_TTL: u8 = 2;
pub(crate) const VERSION_ABSOLUTE_TTL: u8 = 3;

// Record opcodes (one per value type). Each record is:
//   [op][ttl: u8 flag + optional u64][key][type payload]
pub(crate) const OP_EOF: u8 = 0;
pub(crate) const OP_STR: u8 = 1;
pub(crate) const OP_HASH: u8 = 2;
pub(crate) const OP_LIST: u8 = 3;
pub(crate) const OP_SET: u8 = 4;
pub(crate) const OP_ZSET: u8 = 5;
pub(crate) const OP_STREAM: u8 = 6;
/// v2.4 hash field TTL record: `[key][field][deadline_ms: u64 LE]`.
/// Appears only in format v6+ snapshots, after the entry stream's
/// records (before OP_EOF).
pub(crate) const OP_HFTTL: u8 = 7;

/// BufWriter capacity for bulk snapshot / AOF-rewrite writes. The 8 KiB
/// default made SAVE ~12 % of disk bandwidth (tens of thousands of small
/// `write(2)`s); 1 MiB amortizes the syscalls toward disk speed.
pub(crate) const SNAPSHOT_BUF_CAP: usize = 1 << 20;

pub(crate) fn write_ttl<W: Write>(w: &mut W, ttl: Option<u64>) -> io::Result<()> {
    match ttl {
        Some(ms) => {
            w.write_all(&[1u8])?;
            w.write_all(&ms.to_le_bytes())?;
        }
        None => w.write_all(&[0u8])?,
    }
    Ok(())
}

pub(crate) fn read_ttl<R: Read>(r: &mut R) -> io::Result<Option<u64>> {
    if read_u8(r)? == 1 {
        Ok(Some(read_u64(r)?))
    } else {
        Ok(None)
    }
}

pub(crate) fn write_bytes<W: Write>(w: &mut W, b: &[u8]) -> io::Result<()> {
    w.write_all(&(b.len() as u32).to_le_bytes())?;
    w.write_all(b)
}

pub(crate) fn read_bytes<R: Read>(r: &mut R) -> io::Result<Vec<u8>> {
    let len = read_u32(r)? as usize;
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    Ok(buf)
}

pub(crate) fn read_u8<R: Read>(r: &mut R) -> io::Result<u8> {
    let mut b = [0u8; 1];
    r.read_exact(&mut b)?;
    Ok(b[0])
}

pub(crate) fn read_u32<R: Read>(r: &mut R) -> io::Result<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_le_bytes(b))
}

pub(crate) fn read_u64<R: Read>(r: &mut R) -> io::Result<u64> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b)?;
    Ok(u64::from_le_bytes(b))
}
