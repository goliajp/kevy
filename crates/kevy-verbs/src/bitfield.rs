//! `BITFIELD` / `BITFIELD_RO`: the whole argument list is parsed before
//! any operation runs, so a malformed later operation refuses the call.

use kevy_resp::{ArgvView, encode_array_len, encode_error, encode_integer, encode_null_bulk};
use kevy_store::{BitFieldOp, BitType, Overflow, Store};

use crate::args::arg_i64;
use crate::reply::{ERR_NOT_INT, ERR_SYNTAX, store_err, wrong_args};
use crate::{Effect, changed};

const ERR_TYPE: &str = "ERR Invalid bitfield type. Use something like i16 u8. Note that u64 is not supported but i64 is.";
const ERR_OFFSET: &str = "ERR bit offset is not an integer or out of range";
/// The furthest bit a field may start at: a 512 MB string.
const MAX_OFFSET: u64 = 512 * 1024 * 1024 * 8;

/// One command of this group; `None` = not in the group.
pub(crate) fn exec<A: ArgvView + ?Sized>(
    cmd: &[u8],
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
) -> Option<Effect> {
    let read_only = match cmd {
        b"BITFIELD" => false,
        b"BITFIELD_RO" => true,
        _ => return None,
    };
    if args.len() < 2 {
        wrong_args(out, if read_only { "bitfield_ro" } else { "bitfield" });
        return Some(Effect::Unchanged);
    }
    let ops = match parse(args, read_only) {
        Ok(ops) => ops,
        Err(e) => {
            encode_error(out, e);
            return Some(Effect::Unchanged);
        }
    };
    Some(match store.bitfield(&args[1], &ops) {
        Err(e) => {
            store_err(out, e);
            Effect::Unchanged
        }
        Ok((got, wrote)) => {
            encode_array_len(out, got.len() as i64);
            for v in got {
                match v {
                    Some(n) => encode_integer(out, n),
                    None => encode_null_bulk(out),
                }
            }
            if read_only { Effect::Read } else { changed(wrote) }
        }
    })
}

fn parse<A: ArgvView + ?Sized>(args: &A, read_only: bool) -> Result<Vec<BitFieldOp>, &'static str> {
    let (mut ops, mut of, mut i) = (Vec::new(), Overflow::Wrap, 2);
    while i < args.len() {
        let sub = &args[i];
        let operands = if sub.eq_ignore_ascii_case(b"GET") || sub.eq_ignore_ascii_case(b"OVERFLOW")
        {
            usize::from(!sub.eq_ignore_ascii_case(b"OVERFLOW")) + 1
        } else if sub.eq_ignore_ascii_case(b"SET") || sub.eq_ignore_ascii_case(b"INCRBY") {
            3
        } else {
            return Err(ERR_SYNTAX);
        };
        if i + operands >= args.len() {
            return Err(ERR_SYNTAX);
        }
        if sub.eq_ignore_ascii_case(b"OVERFLOW") {
            of = overflow(&args[i + 1])?;
        } else if read_only && !sub.eq_ignore_ascii_case(b"GET") {
            return Err("ERR BITFIELD_RO only supports the GET subcommand");
        } else {
            let t = BitType::parse(&args[i + 1]).ok_or(ERR_TYPE)?;
            let off = offset(&args[i + 2], t)?;
            ops.push(if sub.eq_ignore_ascii_case(b"GET") {
                BitFieldOp::Get(t, off)
            } else {
                let v = arg_i64(&args[i + 3]).ok_or(ERR_NOT_INT)?;
                if sub.eq_ignore_ascii_case(b"SET") {
                    BitFieldOp::Set(t, off, v, of)
                } else {
                    BitFieldOp::IncrBy(t, off, v, of)
                }
            });
        }
        i += operands + 1;
    }
    Ok(ops)
}

fn overflow(b: &[u8]) -> Result<Overflow, &'static str> {
    [(&b"WRAP"[..], Overflow::Wrap), (b"SAT", Overflow::Sat), (b"FAIL", Overflow::Fail)]
        .into_iter()
        .find(|(w, _)| b.eq_ignore_ascii_case(w))
        .map(|(_, o)| o)
        .ok_or("ERR Invalid OVERFLOW type specified")
}

/// A bit offset, or `#n` for the `n`th field of this type's width.
fn offset(b: &[u8], t: BitType) -> Result<u64, &'static str> {
    let (fields, digits) = match b.strip_prefix(b"#") {
        Some(rest) => (true, rest),
        None => (false, b),
    };
    let n: u64 =
        core::str::from_utf8(digits).ok().and_then(|s| s.parse().ok()).ok_or(ERR_OFFSET)?;
    let off = if fields { n.checked_mul(u64::from(t.bits())).ok_or(ERR_OFFSET)? } else { n };
    if off > MAX_OFFSET {
        return Err(ERR_OFFSET);
    }
    Ok(off)
}
