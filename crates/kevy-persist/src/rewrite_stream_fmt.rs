//! Stream AOF-rewrite encoding — split from `rewrite_fmt.rs` when the
//! seg-collection arms pushed it against the 500-LOC cap. The XADD /
//! id-fixup / group reconstruction command stream for one stream value.

use crate::rewrite_fmt::emit;
use kevy_resp::Argv;
use kevy_store::{StreamData, StreamId};
use std::io::{self, Write};


/// Render one stream as commands: one XADD per entry (slow on huge
/// streams but correct — a multi-entry XADD batch is a future parser
/// feature), then `XSETID` whenever a bare replay of those XADDs would
/// not reproduce the scalar state (deleted tail, deleted-only stream,
/// non-zero `entries_added` drift), then the consumer-group section.
/// Returns the number of command frames written so callers that ship
/// rebuild frames elsewhere (scope migration) can report a frame count.
pub fn write_stream_as_commands<W: Write>(w: &mut W, key: &[u8], s: &StreamData) -> io::Result<usize> {
    stream_as_commands(w, key, s, crate::AofFormat::V1, &mut Vec::new())
}

/// Format-aware body of [`write_stream_as_commands`].
pub(crate) fn stream_as_commands<W: Write>(
    w: &mut W,
    key: &[u8],
    s: &StreamData,
    fmt: crate::AofFormat,
    scratch: &mut Vec<u8>,
) -> io::Result<usize> {
    let mut frames = 0usize;
    for (id, fv) in s.iter_entries() {
        let mut argv: Vec<Vec<u8>> = Vec::with_capacity(3 + fv.len() * 2);
        argv.push(b"XADD".to_vec());
        argv.push(key.to_vec());
        argv.push(id.encode());
        for (f, v) in fv {
            argv.push(f.to_vec());
            argv.push(v.to_vec());
        }
        emit(w, &Argv::from(argv), fmt, scratch)?;
        frames += 1;
    }
    frames += write_stream_id_fixup(w, key, s, fmt, scratch)?;
    frames += write_stream_group_commands(w, key, s, fmt, scratch)?;
    Ok(frames)
}

/// The stream-ID bookkeeping tail of a stream rewrite: recreate an
/// emptied stream's advanced ID clock, then XSETID when the natural
/// replay outcome differs from the stored scalars.
pub(crate) fn write_stream_id_fixup<W: Write>(
    w: &mut W,
    key: &[u8],
    s: &StreamData,
    fmt: crate::AofFormat,
    scratch: &mut Vec<u8>,
) -> io::Result<usize> {
    let mut frames = 0usize;
    let (len, last, mxd, added) =
        (s.length(), s.last_id(), s.max_deleted_id(), s.entries_added());
    if len == 0 && last != StreamId::MIN {
        // Empty stream whose ID clock advanced (all entries deleted):
        // re-create the key with the right `last_id` via the
        // `XADD MAXLEN 0` trick — the inline trim wipes the dummy row.
        let argv = vec![
            b"XADD".to_vec(), key.to_vec(), b"MAXLEN".to_vec(), b"0".to_vec(),
            last.encode(), b"x".to_vec(), b"x".to_vec(),
        ];
        emit(w, &Argv::from(argv), fmt, scratch)?;
        frames += 1;
    }
    // What replaying the commands emitted so far yields. The only no-key
    // case left is the virgin empty stream (groups-only) — its scalars
    // are all zero by construction, so skipping XSETID there is exact.
    let natural = if len > 0 {
        (s.last_entry().map_or(StreamId::MIN, |(id, _)| id), len, StreamId::MIN)
    } else {
        (last, u64::from(last != StreamId::MIN), last)
    };
    if natural != (last, added, mxd) {
        let argv = vec![
            b"XSETID".to_vec(), key.to_vec(), last.encode(),
            b"ENTRIESADDED".to_vec(), added.to_string().into_bytes(),
            b"MAXDELETEDID".to_vec(), mxd.encode(),
        ];
        emit(w, &Argv::from(argv), fmt, scratch)?;
        frames += 1;
    }
    Ok(frames)
}

/// Consumer-group section of a stream rewrite: `XGROUP CREATE … MKSTREAM`
/// (MKSTREAM covers groups on a virgin empty stream), one CREATECONSUMER
/// per known consumer, then one `XCLAIM … TIME t RETRYCOUNT n FORCE JUSTID`
/// per live PEL row — full delivery_time/count fidelity, the same technique
/// Redis's own AOF rewrite uses. Tombstone PEL rows (entry XDEL'd while
/// pending) are skipped: XCLAIM purges rather than re-creates those, so
/// only the snapshot path preserves them (accepted trade-off — no RESP
/// verb can recreate a PEL row for a deleted entry).
pub(crate) fn write_stream_group_commands<W: Write>(
    w: &mut W,
    key: &[u8],
    s: &StreamData,
    fmt: crate::AofFormat,
    scratch: &mut Vec<u8>,
) -> io::Result<usize> {
    let mut frames = 0usize;
    for g in s.export_groups() {
        let last_delivered =
            StreamId { ms: g.last_delivered.0, seq: g.last_delivered.1 };
        let argv = vec![
            b"XGROUP".to_vec(), b"CREATE".to_vec(), key.to_vec(), g.name.clone(),
            last_delivered.encode(), b"MKSTREAM".to_vec(),
        ];
        emit(w, &Argv::from(argv), fmt, scratch)?;
        frames += 1;
        for (consumer, _last_seen_ms) in &g.consumers {
            let argv = vec![
                b"XGROUP".to_vec(), b"CREATECONSUMER".to_vec(), key.to_vec(),
                g.name.clone(), consumer.clone(),
            ];
            emit(w, &Argv::from(argv), fmt, scratch)?;
            frames += 1;
        }
        for (ms, seq, consumer, delivery_time_ms, delivery_count) in &g.pel {
            let id = StreamId { ms: *ms, seq: *seq };
            if !s.contains_entry(id) {
                continue;
            }
            let argv = vec![
                b"XCLAIM".to_vec(), key.to_vec(), g.name.clone(), consumer.clone(),
                b"0".to_vec(), id.encode(),
                b"TIME".to_vec(), delivery_time_ms.to_string().into_bytes(),
                b"RETRYCOUNT".to_vec(), delivery_count.to_string().into_bytes(),
                b"FORCE".to_vec(), b"JUSTID".to_vec(),
            ];
            emit(w, &Argv::from(argv), fmt, scratch)?;
            frames += 1;
        }
    }
    Ok(frames)
}
