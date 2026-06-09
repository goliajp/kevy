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
use kevy_store::{Store, Value};
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::Path;

/// Write `store`'s current state to `path` as a sequence of mutating RESP
/// commands prefixed with [`crate::aof::AOF_MAGIC`]; flush + fsync before
/// returning. Returns `(keys, bytes)`. The magic header is consistent with
/// `Aof::open`'s fresh-file behavior so BGREWRITEAOF-produced files replay
/// the same way live-appended ones do.
pub(crate) fn dump_store_to_aof(path: &Path, store: &Store) -> io::Result<(u64, u64)> {
    let f = File::create(path)?;
    let mut w = BufWriter::with_capacity(SNAPSHOT_BUF_CAP, f);
    w.write_all(crate::aof::AOF_MAGIC)?;
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
    let inner = w
        .into_inner()
        .map_err(|e| io::Error::other(e.to_string()))?;
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
        Value::Stream(s) => {
            // One XADD per entry — slow on huge streams but correct.
            // Sprint A trade-off; v2-7c can group MAXLEN once and pump
            // multi-entry XADD batches once the parser handles them.
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
        }
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
pub(crate) fn estimate_multibulk_bytes<A: ArgvView + ?Sized>(args: &A) -> u64 {
    let mut n: u64 = 3 + decimal_digits(args.len() as u64) as u64;
    for i in 0..args.len() {
        let a = &args[i];
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
