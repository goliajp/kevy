//! AOF-rewrite serialization: render the live keyspace as the minimal set of
//! RESP write commands that reconstruct it (`BGREWRITEAOF`'s output), plus the
//! shared multi-bulk frame writer / size estimator the live append path uses.
//!
//! Split out of `lib.rs` (the binary-snapshot format) to keep both files under
//! the 500-LOC house cap. TTL is emitted as an absolute `PEXPIREAT` deadline
//! so a replay reconstructs the original instant rather than
//! re-anchoring to replay-time (re-anchoring silently extends every
//! TTL by the key's age — a production incident class).

use crate::SNAPSHOT_BUF_CAP;
use kevy_resp::{Argv, ArgvView};

use crate::rewrite_chunk::write_verb_items;
use kevy_store::{StreamData, StreamId, Value};
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::Path;

/// Write `src`'s state (a live `Store` or a frozen
/// [`kevy_store::SnapshotView`]) to `path` as a sequence of mutating RESP
/// commands prefixed with `crate::aof::AOF_MAGIC`; flush + fsync before
/// returning. Returns `(keys, bytes)`. The magic header is consistent with
/// `Aof::open`'s fresh-file behavior so BGREWRITEAOF-produced files replay
/// the same way live-appended ones do.
///
/// `pub` (not just crate-internal) because the COW rewrite path calls it
/// from a background thread: [`crate::Aof::begin_view_rewrite`] starts the
/// tee, this serializes the frozen view to the temp file off-thread, and
/// `finish_concurrent_rewrite` swaps it in.
/// Write one command in the image's format: bare multibulk (v1) or a
/// checksummed record envelope (v2).
pub(crate) fn emit<W: Write, A: kevy_resp::ArgvView + ?Sized>(
    w: &mut W,
    args: &A,
    fmt: crate::AofFormat,
    scratch: &mut Vec<u8>,
) -> io::Result<()> {
    match fmt {
        crate::AofFormat::V1 => write_multibulk(w, args),
        crate::AofFormat::V2 => crate::record::write_record_multibulk(w, args, scratch),
    }
}

/// Serialize `src`'s whole state to a fresh compacted AOF at `path`
/// (fsynced): the rewrite image — always the v2 checksummed-record
/// format. Returns `(keys, bytes)`.
pub fn dump_aof<S: crate::SnapshotSource>(path: &Path, src: &S) -> io::Result<(u64, u64)> {
    // Drop-behind stride — a multi-GB image left dirty floods the page
    // cache into direct reclaim inside the server's own fault paths
    // (5.2M pages scanned vs 6.6k without; the S5-E/F finding).
    const DROP_BEHIND: u64 = 64 << 20;
    let f = File::create(path)?;
    let mut w = BufWriter::with_capacity(SNAPSHOT_BUF_CAP, f);
    let mut scratch = Vec::new();
    w.write_all(crate::record::AOF2_MAGIC)?;
    let mut keys = 0u64;
    let mut err: Option<io::Error> = None;
    let mut cold_seqs: Vec<u32> = Vec::new();
    let mut since_drop: u64 = 0;
    src.for_each_entry(|key, value, ttl_ms| {
        if err.is_some() {
            return;
        }
        // Seg-backed stub: the row's data does NOT re-enter the log —
        // its segment's trailing SEGMENTED frame re-establishes the
        // stub at replay (demote-or-insert).
        if let kevy_store::Value::Cold(c) = value
            && let Some((seq, _)) = c.seg_parts()
        {
            if !cold_seqs.contains(&seq) {
                cold_seqs.push(seq);
            }
            return;
        }
        if let Err(e) =
            write_value_as_commands(&mut w, key, value, ttl_ms, crate::AofFormat::V2, &mut scratch)
        {
            err = Some(e);
        } else {
            keys += 1;
            // scratch underestimates multi-frame values ⇒ drops only more often.
            since_drop = since_drop.saturating_add(scratch.len() as u64);
            if since_drop >= DROP_BEHIND {
                since_drop = 0;
                crate::dump_cache::drop_behind_step(&mut w);
            }
        }
    });
    if let Some(e) = err {
        return Err(e);
    }
    finish_dump(w, src, &cold_seqs, &mut scratch, keys)
}

/// `dump_aof`'s tail: trailing hash-TTL + SEGMENTED frames, final
/// flush + fsync, size readout. Split for the 50-LOC rule.
fn finish_dump<S: crate::SnapshotSource>(
    mut w: BufWriter<File>,
    src: &S,
    cold_seqs: &[u32],
    scratch: &mut Vec<u8>,
    keys: u64,
) -> io::Result<(u64, u64)> {
    write_hash_ttl_frames(&mut w, src, crate::AofFormat::V2, scratch)?;
    crate::segmented::write_segmented_frames(&mut w, src, cold_seqs, scratch)?;
    w.flush()?;
    let inner = w
        .into_inner()
        .map_err(|e| io::Error::other(e.to_string()))?;
    let bytes = inner.metadata().map_or(0, |m| m.len());
    inner.sync_all()?;
    Ok((keys, bytes))
}

/// Hash field TTLs re-emitted as absolute HPEXPIREAT frames (after the
/// HSETs that recreate their fields).
fn write_hash_ttl_frames<W: Write, S: crate::SnapshotSource>(
    w: &mut W,
    src: &S,
    fmt: crate::AofFormat,
    scratch: &mut Vec<u8>,
) -> io::Result<()> {
    let mut err: Option<io::Error> = None;
    src.for_each_hash_ttl(|key, field, deadline_ms| {
        if err.is_some() {
            return;
        }
        let ms = deadline_ms.to_string();
        let mut argv = kevy_resp::Argv::with_capacity(6, 0);
        argv.push(b"HPEXPIREAT");
        argv.push(key);
        argv.push(ms.as_bytes());
        argv.push(b"FIELDS");
        argv.push(b"1");
        argv.push(field);
        if let Err(e) = emit(w, &argv, fmt, scratch) {
            err = Some(e);
        }
    });
    match err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// Serialize `src`'s state into an in-memory AOF image (magic + the same
/// RESP command stream [`dump_aof`] writes). Returns the bytes and the key
/// count. Used by the non-blocking rewrite (the caller produces this buffer
/// under the store lock, then spills it to disk *off* the lock) and by
/// host-mediated persistence (targets without a filesystem hand the image
/// to the host to store). `Vec<u8>` is an infallible `Write`, so no error
/// path exists.
pub fn dump_store_to_buf<S: crate::SnapshotSource>(
    src: &S,
    fmt: crate::AofFormat,
) -> (Vec<u8>, u64) {
    let mut buf = Vec::with_capacity(crate::SNAPSHOT_BUF_CAP);
    buf.extend_from_slice(match fmt {
        crate::AofFormat::V1 => crate::aof::AOF_MAGIC,
        crate::AofFormat::V2 => crate::record::AOF2_MAGIC,
    });
    let mut scratch = Vec::new();
    let mut keys = 0u64;
    src.for_each_entry(|key, value, ttl_ms| {
        let _ = write_value_as_commands(&mut buf, key, value, ttl_ms, fmt, &mut scratch);
        keys += 1;
    });
    (buf, keys)
}

/// Emit one (or two, if TTL'd) RESP write commands that, when replayed,
/// reconstruct `key`'s `value` and TTL exactly.
///
/// C6: marked `#[cold]` — AOF rewrite only runs on `BGREWRITEAOF`
/// or `auto-aof-rewrite-percentage`, never on the live write path.
/// The full type-switch over `Value` variants is ~9 KB; pushing it
/// off the hot iTLB pages around `start_command` is the point.
#[cold]
// LOC-WAIVER: pure per-Value-variant dispatch table — one arm per
// stored type mapping it to its canonical rewrite verb; no control flow.
pub(crate) fn write_value_as_commands<W: Write>(
    w: &mut W,
    key: &[u8],
    value: &Value,
    ttl_ms: Option<u64>,
    fmt: crate::AofFormat,
    scratch: &mut Vec<u8>,
) -> io::Result<()> {
    match value {
        // Every SnapshotSource materializes cold values from the
        // (pinned) vlog before yielding them, so a stub here means a
        // producer bypassed the source contract. Failing the rewrite is
        // the only honest outcome — a skip would silently drop the value
        // from the sole durability truth.
        Value::Cold(_) => {
            return Err(io::Error::other(
                "cold stub reached the AOF rewrite — SnapshotSource must materialize (T4)",
            ));
        }
        Value::Str(s) => write_verb_items(w, b"SET", key, 1, [s.to_vec()], fmt, scratch)?,
        // L2: persist Int as the canonical ASCII bytes; replay's SET
        // auto-detects it back to Int via parse_canonical_i64.
        Value::Int(n) => write_verb_items(w, b"SET", key, 1, [n.to_string().into_bytes()], fmt, scratch)?,
        // L1: Arc-bulk serialises via the same SET argv path; replay picks
        // ArcBulk again for > BULK_THRESHOLD bytes.
        Value::ArcBulk(a) => write_verb_items(w, b"SET", key, 1, [a.as_ref().to_vec()], fmt, scratch)?,
        Value::Hash(h) => {
            let fv = h.iter().flat_map(|(f, v)| [f.to_vec(), v.to_vec()]);
            write_verb_items(w, b"HSET", key, 2, fv, fmt, scratch)?;
        }
        // A.8: inline hash / list / zset rewrite to the same HSET / RPUSH
        // / ZADD forms as their heap-backed twins; replay re-runs the
        // encoding switch so small values land inline again.
        Value::SmallHashInline(h) => {
            let fv = h.iter().flat_map(|(f, v)| [f.to_vec(), v.to_vec()]);
            write_verb_items(w, b"HSET", key, 2, fv, fmt, scratch)?;
        }
        Value::List(l) => write_verb_items(w, b"RPUSH", key, 1, l.iter().cloned(), fmt, scratch)?,
        Value::SegList(l) => {
            write_verb_items(w, b"RPUSH", key, 1, l.iter().cloned(), fmt, scratch)?;
        }
        Value::SmallListInline(l) => {
            write_verb_items(w, b"RPUSH", key, 1, l.iter().map(<[u8]>::to_vec), fmt, scratch)?;
        }
        // A.7 O5: inline-encoded set rewrites to the same SADD command form
        // as the heap-backed `Value::Set`; replaying through the live SADD
        // handler re-runs the encoding switch (small → inline, big → KevySet).
        Value::Set(s) => {
            write_verb_items(w, b"SADD", key, 1, s.iter().map(kevy_store::SmallBytes::to_vec), fmt, scratch)?;
        }
        Value::SmallSetInline(s) => {
            write_verb_items(w, b"SADD", key, 1, s.iter().map(<[u8]>::to_vec), fmt, scratch)?;
        }
        Value::ZSet(z) => {
            let ms = z.ordered().flat_map(|(m, sc)| [fmt_zset_score(sc), m.to_vec()]);
            write_verb_items(w, b"ZADD", key, 2, ms, fmt, scratch)?;
        }
        Value::SmallZSetInline(z) => {
            let ms = z.iter().flat_map(|(m, sc)| [fmt_zset_score(sc), m.to_vec()]);
            write_verb_items(w, b"ZADD", key, 2, ms, fmt, scratch)?;
        }
        Value::Stream(s) => _ = stream_as_commands(w, key, s, fmt, scratch)?,
    }
    write_pexpireat(w, key, ttl_ms, fmt, scratch)
}

/// `ms` is remaining; emit an absolute `PEXPIREAT` deadline so a replay
/// of the rewritten AOF reconstructs the original instant instead of
/// re-anchoring to replay-time (which would silently extend the TTL).
fn write_pexpireat<W: Write>(
    w: &mut W,
    key: &[u8],
    ttl_ms: Option<u64>,
    fmt: crate::AofFormat,
    scratch: &mut Vec<u8>,
) -> io::Result<()> {
    let Some(ms) = ttl_ms else { return Ok(()) };
    let deadline = kevy_store::now_unix_ms().saturating_add(ms);
    write_verb_items(w, b"PEXPIREAT", key, 1, [deadline.to_string().into_bytes()], fmt, scratch)
}

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
fn stream_as_commands<W: Write>(
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
fn write_stream_id_fixup<W: Write>(
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
fn write_stream_group_commands<W: Write>(
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

/// Format a sorted-set score the way Redis does (no trailing `.0` for
/// integers; up to 17 sig figs for non-integer doubles). Tests want the
/// replay-roundtrip to compare byte-equal, so don't introduce locale
/// differences (`format!` is locale-free here).
fn fmt_zset_score(s: f64) -> Vec<u8> {
    // Bit-exact compare is the contract — "no fractional bits in the f64",
    // not "approximately integer". An epsilon would mis-classify near-int
    // values as integers and change wire bytes.
    #[allow(clippy::float_cmp)]
    let is_integer_valued = s.is_finite() && s == s.trunc();
    if is_integer_valued && s.abs() < 1e17 {
        format!("{}", s as i64).into_bytes()
    } else {
        format!("{s:.17}").into_bytes()
    }
}

/// Cheap byte-count estimator for a single multi-bulk frame:
/// `*<n>\r\n` + per-arg `$<len>\r\n<bytes>\r\n`. No allocation, no
/// double-pass — accurate to within a couple of bytes per arg.
pub(crate) fn estimate_multibulk_bytes<A: ArgvView + ?Sized>(args: &A) -> u64 {
    let mut n: u64 = 3 + u64::from(decimal_digits(args.len() as u64));
    for i in 0..args.len() {
        let a = &args[i];
        n += 3 + u64::from(decimal_digits(a.len() as u64)) + a.len() as u64 + 2;
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

/// Encode one command as a RESP multi-bulk frame (`*<n>\r\n` then
/// `$<len>\r\n<bytes>\r\n` per argument) — the exact byte shape
/// [`Aof::append`](crate::Aof::append) writes and
/// [`replay_aof`](crate::replay_aof) parses back. Public so external AOF
/// producers (host-mediated persistence pumps) emit frames byte-compatible
/// with kevy-written logs.
pub fn write_multibulk<W: Write, A: ArgvView + ?Sized>(
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

/// Parity manifest: every verb the AOF rewrite emits.
/// Cross-checked against `kevy_resp::ops_table` below.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const REWRITE_EMIT_VERBS: &[&str] = &[
    "SET", "HSET", "RPUSH", "SADD", "ZADD", "PEXPIREAT", "HPEXPIREAT",
    "XADD", "XSETID", "XGROUP", "XCLAIM",
];

#[cfg(test)]
mod op_table_parity {
    use super::REWRITE_EMIT_VERBS;
    use kevy_resp::ops_table::{ops_with, surface};
    use std::collections::BTreeSet;

    #[test]
    fn rewrite_manifest_matches_table() {
        let m: BTreeSet<&str> = REWRITE_EMIT_VERBS.iter().copied().collect();
        let t: BTreeSet<&str> = ops_with(surface::REWRITE).into_iter().collect();
        assert_eq!(
            m, t,
            "rewrite emit set != OP_TABLE REWRITE flags — update both when changing rewrite_fmt"
        );
    }

    #[test]
    fn rewrite_manifest_verbs_have_source_literals() {
        let src = include_str!("rewrite_fmt.rs");
        for v in REWRITE_EMIT_VERBS {
            let lit = format!("\"{v}");
            assert!(
                src.contains(&lit) || src.contains(&format!("b\"{v}\"")),
                "REWRITE_EMIT_VERBS lists {v} but rewrite_fmt.rs has no literal for it"
            );
        }
    }
}
