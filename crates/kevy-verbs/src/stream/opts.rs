//! Argument pieces several stream commands share: integers read the way a
//! Redis server reads them, the trim options of `XADD` / `XTRIM`, and the
//! bounds of an ID interval, `(` making one exclusive.

use kevy_resp::{ArgvView, CmdError};
use kevy_store::{
    APPROX_TRIM_LIMIT, StreamId, TrimMode, TrimTo, parse_range_end, parse_range_start,
};

pub(super) const BAD_ID: &str = "ERR Invalid stream ID specified as stream command argument";

/// A decimal `i64` with no `+`, no leading zero and no `-0`: `-7` reads
/// as -7, while `07`, `+7` and `-0` are refused.
pub(super) fn strict_i64(b: &[u8]) -> Option<i64> {
    let digits = b.strip_prefix(b"-").unwrap_or(b);
    let ok = !digits.is_empty()
        && digits.len() <= 20
        && digits.iter().all(u8::is_ascii_digit)
        && (digits[0] != b'0' || b == b"0");
    if !ok {
        return None;
    }
    std::str::from_utf8(b).ok()?.parse().ok()
}

/// The trim a command asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Trim {
    pub(super) to: TrimTo,
    pub(super) mode: TrimMode,
}

/// `MAXLEN|MINID [=|~] threshold` and `LIMIT n`, met in any order.
#[derive(Default)]
pub(super) struct TrimParser {
    trim: Option<(TrimTo, bool)>,
    limit: Option<usize>,
}

impl TrimParser {
    /// Take the option at `args[i]` if it is a trim option: `Some(n)` args
    /// consumed, `None` when it is not one.
    pub(super) fn take<A: ArgvView + ?Sized>(
        &mut self,
        args: &A,
        i: usize,
    ) -> Result<Option<usize>, CmdError> {
        let tok = &args[i];
        // an option is one only when a value follows it
        let Some(next) = args.get(i + 1) else {
            return Ok(None);
        };
        if tok.eq_ignore_ascii_case(b"LIMIT") {
            let n = strict_i64(next).ok_or(CmdError::Wire(crate::reply::ERR_NOT_INT))?;
            let n = usize::try_from(n)
                .map_err(|_| CmdError::Wire("ERR The LIMIT argument must be >= 0."))?;
            self.limit = Some(n);
            return Ok(Some(2));
        }
        let maxlen = tok.eq_ignore_ascii_case(b"MAXLEN");
        if !maxlen && !tok.eq_ignore_ascii_case(b"MINID") {
            return Ok(None);
        }
        if self.trim.is_some() {
            return Err(CmdError::Wire(
                "ERR syntax error, MAXLEN and MINID options at the same time are not compatible",
            ));
        }
        // a modifier is one only when a threshold follows it
        let (approx, v, taken) = match args.get(i + 2) {
            Some(v) if next == b"=" || next == b"~" => (next == b"~", v, 3),
            _ => (false, next, 2),
        };
        let to = if maxlen {
            let n = strict_i64(v).ok_or(CmdError::Wire(crate::reply::ERR_NOT_INT))?;
            let n = u64::try_from(n)
                .map_err(|_| CmdError::Wire("ERR The MAXLEN argument must be >= 0."))?;
            TrimTo::MaxLen(n)
        } else {
            TrimTo::MinId(kevy_store::parse_explicit_id(v).map_err(|_| CmdError::Wire(BAD_ID))?)
        };
        self.trim = Some((to, approx));
        Ok(Some(taken))
    }

    /// The trim, once every option is in.
    pub(super) fn finish(self) -> Result<Option<Trim>, CmdError> {
        match (self.trim, self.limit) {
            (None, Some(_)) => Err(CmdError::Wire(
                "ERR syntax error, LIMIT cannot be used without specifying a trimming strategy",
            )),
            (Some((_, false)), Some(_)) => Err(CmdError::Wire(
                "ERR syntax error, LIMIT cannot be used without the special ~ option",
            )),
            (None, None) => Ok(None),
            (Some((to, approx)), limit) => {
                let mode = if approx {
                    TrimMode::Approximate { limit: limit.unwrap_or(APPROX_TRIM_LIMIT) }
                } else {
                    TrimMode::Exact
                };
                Ok(Some(Trim { to, mode }))
            }
        }
    }
}

/// The start of an interval: `-`, `+`, an ID, or `(` and an ID to start
/// just after it.
pub(super) fn interval_start(s: &[u8]) -> Result<StreamId, CmdError> {
    let Some(id) = s.strip_prefix(b"(") else {
        return parse_range_start(s).map_err(|_| CmdError::Wire(BAD_ID));
    };
    let id = exclusive_id(id)?;
    if id == StreamId::MAX {
        return Err(CmdError::Wire("ERR invalid start ID for the interval"));
    }
    Ok(id.next())
}

/// The end of an interval: `+`, `-`, an ID, or `(` and an ID to end just
/// before it.
pub(super) fn interval_end(s: &[u8]) -> Result<StreamId, CmdError> {
    let Some(id) = s.strip_prefix(b"(") else {
        return parse_range_end(s).map_err(|_| CmdError::Wire(BAD_ID));
    };
    let id = exclusive_id(id)?;
    if id == StreamId::MIN {
        return Err(CmdError::Wire("ERR invalid end ID for the interval"));
    }
    Ok(if id.seq > 0 {
        StreamId::new(id.ms, id.seq - 1)
    } else {
        StreamId::new(id.ms - 1, u64::MAX)
    })
}

/// The ID after `(`: never `-` or `+`.
fn exclusive_id(s: &[u8]) -> Result<StreamId, CmdError> {
    if s == b"-" || s == b"+" {
        return Err(CmdError::Wire(BAD_ID));
    }
    kevy_store::parse_explicit_id(s).map_err(|_| CmdError::Wire(BAD_ID))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integers_read_as_a_redis_server_reads_them() {
        assert_eq!(strict_i64(b"-7"), Some(-7));
        assert_eq!(strict_i64(b"0"), Some(0));
        for not in [&b"07"[..], b"+7", b"-0", b" 7", b"7 ", b"", b"-", b"9223372036854775808"] {
            assert_eq!(strict_i64(not), None, "{not:?}");
        }
    }

    #[test]
    fn exclusive_bounds() {
        let id = StreamId::new;
        assert_eq!(interval_start(b"(1-5"), Ok(id(1, 6)));
        assert_eq!(interval_start(b"(1"), Ok(id(1, 1)));
        assert_eq!(interval_end(b"(3-0"), Ok(id(2, u64::MAX)));
        assert_eq!(interval_start(b"+"), Ok(StreamId::MAX));
        let max = b"(18446744073709551615-18446744073709551615";
        assert_eq!(
            interval_start(max),
            Err(CmdError::Wire("ERR invalid start ID for the interval"))
        );
        assert_eq!(
            interval_end(b"(0-0"),
            Err(CmdError::Wire("ERR invalid end ID for the interval"))
        );
        assert_eq!(interval_start(b"(-"), Err(CmdError::Wire(BAD_ID)));
    }
}
