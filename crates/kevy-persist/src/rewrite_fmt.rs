//! AOF-rewrite serialization: render the live keyspace as the minimal set of
//! RESP write commands that reconstruct it (`BGREWRITEAOF`'s output), plus the
//! shared multi-bulk frame writer / size estimator the live append path uses.
//!
//! Split out of `lib.rs` (the binary-snapshot format) to keep both files under
//! the 500-LOC house cap. TTL is emitted as an absolute `PEXPIREAT` deadline
//! so a replay reconstructs the original instant (INC-2026-06-09) rather than
//! re-anchoring to replay-time.

use crate::SNAPSHOT_BUF_CAP;
use kevy_resp::{Argv, ArgvView};
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
pub fn dump_aof<S: crate::SnapshotSource>(path: &Path, src: &S) -> io::Result<(u64, u64)> {
    let f = File::create(path)?;
    let mut w = BufWriter::with_capacity(SNAPSHOT_BUF_CAP, f);
    w.write_all(crate::aof::AOF_MAGIC)?;
    let mut keys = 0u64;
    let mut err: Option<io::Error> = None;
    src.for_each_entry(|key, value, ttl_ms| {
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
    let bytes = inner.metadata().map_or(0, |m| m.len());
    inner.sync_all()?;
    Ok((keys, bytes))
}

/// Serialize `src`'s state into an in-memory AOF image (magic + the same
/// RESP command stream [`dump_aof`] writes). Returns the bytes and the key
/// count. Used by the non-blocking rewrite: the caller produces this buffer
/// under the store lock, then spills it to disk *off* the lock. `Vec<u8>`
/// is an infallible `Write`, so no error path exists.
pub(crate) fn dump_store_to_buf<S: crate::SnapshotSource>(src: &S) -> (Vec<u8>, u64) {
    let mut buf = Vec::with_capacity(crate::SNAPSHOT_BUF_CAP);
    buf.extend_from_slice(crate::aof::AOF_MAGIC);
    let mut keys = 0u64;
    src.for_each_entry(|key, value, ttl_ms| {
        let _ = write_value_as_commands(&mut buf, key, value, ttl_ms);
        keys += 1;
    });
    (buf, keys)
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
        Value::Stream(s) => write_stream_as_commands(w, key, s)?,
    }
    if let Some(ms) = ttl_ms {
        // `ms` is remaining; emit an absolute `PEXPIREAT` deadline so a replay
        // of this rewritten AOF reconstructs the original instant instead of
        // re-anchoring to replay-time (INC-2026-06-09).
        let deadline = kevy_store::now_unix_ms().saturating_add(ms);
        let argv = Argv::from(vec![
            b"PEXPIREAT".to_vec(),
            key.to_vec(),
            deadline.to_string().into_bytes(),
        ]);
        write_multibulk(w, &argv)?;
    }
    Ok(())
}

/// Render one stream as commands: one XADD per entry (slow on huge
/// streams but correct — a multi-entry XADD batch is a future parser
/// feature), then `XSETID` whenever a bare replay of those XADDs would
/// not reproduce the scalar state (deleted tail, deleted-only stream,
/// non-zero `entries_added` drift), then the consumer-group section.
fn write_stream_as_commands<W: Write>(w: &mut W, key: &[u8], s: &StreamData) -> io::Result<()> {
    for (id, fv) in s.iter_entries() {
        let mut argv: Vec<Vec<u8>> = Vec::with_capacity(3 + fv.len() * 2);
        argv.push(b"XADD".to_vec());
        argv.push(key.to_vec());
        argv.push(id.encode());
        for (f, v) in fv {
            argv.push(f.to_vec());
            argv.push(v.to_vec());
        }
        write_multibulk(w, &Argv::from(argv))?;
    }
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
        write_multibulk(w, &Argv::from(argv))?;
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
        write_multibulk(w, &Argv::from(argv))?;
    }
    write_stream_group_commands(w, key, s)
}

/// Consumer-group section of a stream rewrite: `XGROUP CREATE … MKSTREAM`
/// (MKSTREAM covers groups on a virgin empty stream), one CREATECONSUMER
/// per known consumer, then one `XCLAIM … TIME t RETRYCOUNT n FORCE JUSTID`
/// per live PEL row — full delivery_time/count fidelity, the same technique
/// Redis's own AOF rewrite uses. Tombstone PEL rows (entry XDEL'd while
/// pending) are skipped: XCLAIM purges rather than re-creates those, so
/// only the snapshot path preserves them (RFC 2026-06-11 trade-off).
fn write_stream_group_commands<W: Write>(
    w: &mut W,
    key: &[u8],
    s: &StreamData,
) -> io::Result<()> {
    for g in s.export_groups() {
        let last_delivered =
            StreamId { ms: g.last_delivered.0, seq: g.last_delivered.1 };
        let argv = vec![
            b"XGROUP".to_vec(), b"CREATE".to_vec(), key.to_vec(), g.name.clone(),
            last_delivered.encode(), b"MKSTREAM".to_vec(),
        ];
        write_multibulk(w, &Argv::from(argv))?;
        for (consumer, _last_seen_ms) in &g.consumers {
            let argv = vec![
                b"XGROUP".to_vec(), b"CREATECONSUMER".to_vec(), key.to_vec(),
                g.name.clone(), consumer.clone(),
            ];
            write_multibulk(w, &Argv::from(argv))?;
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
            write_multibulk(w, &Argv::from(argv))?;
        }
    }
    Ok(())
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
