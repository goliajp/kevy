//! `XACK` and `XPENDING` (`XREADGROUP` is in `readgroup.rs`, `XGROUP` in
//! `xgroup.rs`, the claims in `claim.rs`).

use kevy_resp::CmdError;
use kevy_resp::{
    ArgvView, encode_array_len, encode_bulk, encode_error, encode_integer, encode_null_bulk,
};
use kevy_store::{Store, StreamId, now_unix_ms, parse_explicit_id};

use crate::reply::{store_err, wrong_args};

use super::opts::{BAD_ID, interval_end, interval_start, strict_i64};

// ───────────── XACK ─────────────

/// `XACK key group id [id ...]`: a missing key or group acknowledges
/// nothing, before any ID is read.
pub(super) fn cmd_xack<A: ArgvView + ?Sized>(store: &mut Store, args: &A, out: &mut Vec<u8>) {
    if args.len() < 4 {
        return wrong_args(out, "xack");
    }
    match store.stream_view(&args[1]) {
        Ok(Some(s)) if s.group(&args[2]).is_some() => {}
        Ok(_) => return encode_integer(out, 0),
        Err(e) => return store_err(out, e),
    }
    let mut ids = Vec::with_capacity(args.len() - 3);
    for i in 3..args.len() {
        match parse_explicit_id(&args[i]) {
            Ok(id) => ids.push(id),
            Err(_) => return encode_error(out, BAD_ID),
        }
    }
    match store.xack(&args[1], &args[2], &ids) {
        Ok(n) => encode_integer(out, n as i64),
        Err(e) => store_err(out, e),
    }
}

// ───────────── XPENDING ─────────────

/// The `NOGROUP` a missing key or group gets from `XPENDING` and the
/// claims.
pub(super) fn no_key_or_group(out: &mut Vec<u8>, key: &[u8], group: &[u8]) {
    encode_error(
        out,
        &format!(
            "NOGROUP No such key '{}' or consumer group '{}'",
            String::from_utf8_lossy(key),
            String::from_utf8_lossy(group),
        ),
    );
}

/// `XPENDING key group [[IDLE min-idle] start end count [consumer]]`:
/// every argument is read before the key is looked at.
pub(super) fn cmd_xpending<A: ArgvView + ?Sized>(store: &mut Store, args: &A, out: &mut Vec<u8>) {
    if args.len() < 3 {
        return wrong_args(out, "xpending");
    }
    let (key, group) = (&args[1], &args[2]);
    if args.len() == 3 {
        match store.xpending_summary(key, group) {
            Ok(Some(s)) => emit_pending_summary(out, &s),
            Ok(None) => no_key_or_group(out, key, group),
            Err(e) => store_err(out, e),
        }
        return;
    }
    let p = match parse_xpending_extended(args) {
        Ok(p) => p,
        Err(msg) => return encode_error(out, msg.as_wire()),
    };
    let now = now_unix_ms();
    let got = store.xpending_extended(key, group, p.idle, p.start, p.end, p.count, p.consumer, now);
    match got {
        Ok(Some(rows)) => emit_pending_extended(out, &rows.rows),
        Ok(None) => no_key_or_group(out, key, group),
        Err(e) => store_err(out, e),
    }
}

struct XPendingExtendedArgs<'a> {
    idle: Option<u64>,
    start: StreamId,
    end: StreamId,
    count: usize,
    consumer: Option<&'a [u8]>,
}

fn parse_xpending_extended<A: ArgvView + ?Sized>(
    args: &A,
) -> Result<XPendingExtendedArgs<'_>, CmdError> {
    let mut i = 3;
    let mut idle = None;
    if args[i].eq_ignore_ascii_case(b"IDLE") {
        let v = args.get(i + 1).ok_or("ERR syntax error")?;
        let v = strict_i64(v).ok_or(CmdError::Wire(crate::reply::ERR_NOT_INT))?;
        // a negative minimum admits every entry
        idle = Some(u64::try_from(v).unwrap_or(0));
        i += 2;
    }
    if args.len() < i + 3 {
        return Err(CmdError::Wire("ERR syntax error"));
    }
    let start = interval_start(&args[i])?;
    let end = interval_end(&args[i + 1])?;
    let count = strict_i64(&args[i + 2]).ok_or(CmdError::Wire(crate::reply::ERR_NOT_INT))?;
    // a negative count lists nothing; anything after the consumer is ignored
    let count = usize::try_from(count).unwrap_or(0);
    let consumer = args.get(i + 3);
    Ok(XPendingExtendedArgs { idle, start, end, count, consumer })
}

fn emit_pending_summary(out: &mut Vec<u8>, s: &kevy_store::PendingSummary) {
    encode_array_len(out, 4);
    encode_integer(out, s.total as i64);
    if let Some((lo, hi)) = s.id_range {
        encode_bulk(out, &lo.encode());
        encode_bulk(out, &hi.encode());
    } else {
        encode_null_bulk(out);
        encode_null_bulk(out);
    }
    if s.by_consumer.is_empty() {
        encode_array_len(out, -1);
        return;
    }
    encode_array_len(out, s.by_consumer.len() as i64);
    for (name, n) in &s.by_consumer {
        encode_array_len(out, 2);
        encode_bulk(out, name);
        encode_bulk(out, n.to_string().as_bytes());
    }
}

fn emit_pending_extended(out: &mut Vec<u8>, rows: &[kevy_store::PendingExtendedRow]) {
    encode_array_len(out, rows.len() as i64);
    for r in rows {
        encode_array_len(out, 4);
        encode_bulk(out, &r.id.encode());
        encode_bulk(out, &r.consumer);
        encode_integer(out, r.idle_ms as i64);
        encode_integer(out, i64::try_from(r.delivery_count).unwrap_or(i64::MAX));
    }
}
