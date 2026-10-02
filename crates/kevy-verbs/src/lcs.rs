//! `LCS`: the longest common subsequence of two strings — the string, its
//! length, or the matching ranges — by the dynamic program Redis runs,
//! walked back from the end so the ranges come out last match first.

use kevy_resp::{
    ArgvView, RespVersion, encode_array_len, encode_bulk, encode_error, encode_integer,
};

use crate::args::arg_i64;
use crate::reply::ERR_SYNTAX;

/// The most table cells Redis builds: its 512 MB bulk limit over 4-byte
/// cells.
const MAX_CELLS: u64 = 512 * 1024 * 1024 / 4;

struct Opts {
    len: bool,
    idx: bool,
    min: u64,
    with_len: bool,
}

fn opts<A: ArgvView + ?Sized>(args: &A) -> Result<Opts, &'static str> {
    let mut o = Opts { len: false, idx: false, min: 0, with_len: false };
    let mut i = 3;
    while i < args.len() {
        let a = &args[i];
        if a.eq_ignore_ascii_case(b"LEN") {
            o.len = true;
        } else if a.eq_ignore_ascii_case(b"IDX") {
            o.idx = true;
        } else if a.eq_ignore_ascii_case(b"WITHMATCHLEN") {
            o.with_len = true;
        } else if a.eq_ignore_ascii_case(b"MINMATCHLEN") && i + 1 < args.len() {
            let n = arg_i64(&args[i + 1]).ok_or("ERR value is not an integer or out of range")?;
            o.min = n.max(0) as u64;
            i += 1;
        } else {
            return Err(ERR_SYNTAX);
        }
        i += 1;
    }
    if o.len && o.idx {
        return Err("ERR If you want both the length and indexes, please just use IDX.");
    }
    Ok(o)
}

/// One matching run: `a[a.0..=a.1]` equals `b[b.0..=b.1]`.
struct Run {
    a: (usize, usize),
    b: (usize, usize),
}

/// Answer `LCS` for strings `a` and `b` (the keys' values, empty when
/// absent) under the options in `args`.
pub(crate) fn lcs_reply<A: ArgvView + ?Sized>(
    args: &A,
    a: &[u8],
    b: &[u8],
    out: &mut Vec<u8>,
    proto: RespVersion,
) {
    let o = match opts(args) {
        Ok(o) => o,
        Err(e) => return encode_error(out, e),
    };
    let cells = (a.len() as u64 + 1).saturating_mul(b.len() as u64 + 1);
    if cells > MAX_CELLS {
        return encode_error(
            out,
            "ERR Insufficient memory, transient memory for LCS exceeds proto-max-bulk-len",
        );
    }
    let (seq, runs) = walk(a, b, o.idx, o.min);
    if o.len {
        return encode_integer(out, seq.len() as i64);
    }
    if !o.idx {
        return encode_bulk(out, &seq);
    }
    if proto == RespVersion::V3 {
        kevy_resp::encode_map_header(out, 2);
    } else {
        encode_array_len(out, 4);
    }
    encode_bulk(out, b"matches");
    encode_array_len(out, runs.len() as i64);
    for r in &runs {
        encode_array_len(out, if o.with_len { 3 } else { 2 });
        for (s, e) in [r.a, r.b] {
            encode_array_len(out, 2);
            encode_integer(out, s as i64);
            encode_integer(out, e as i64);
        }
        if o.with_len {
            encode_integer(out, (r.a.1 - r.a.0 + 1) as i64);
        }
    }
    encode_bulk(out, b"len");
    encode_integer(out, seq.len() as i64);
}

/// The LCS length of every pair of prefixes, row `i` for `a[..i]`.
fn table(a: &[u8], b: &[u8]) -> Vec<u32> {
    let w = b.len() + 1;
    let mut t = vec![0u32; (a.len() + 1) * w];
    for i in 1..=a.len() {
        for j in 1..=b.len() {
            t[i * w + j] = if a[i - 1] == b[j - 1] {
                t[(i - 1) * w + j - 1] + 1
            } else {
                t[(i - 1) * w + j].max(t[i * w + j - 1])
            };
        }
    }
    t
}

/// The subsequence, and — when `idx` — its runs no shorter than `min`.
fn walk(a: &[u8], b: &[u8], idx: bool, min: u64) -> (Vec<u8>, Vec<Run>) {
    let w = b.len() + 1;
    let t = table(a, b);
    let n = t[a.len() * w + b.len()] as usize;
    let mut seq = vec![0u8; n];
    let mut runs = Vec::new();
    let (mut i, mut j, mut k) = (a.len(), b.len(), n);
    // `open` is the run being grown backwards: (a_start, a_end, b_start, b_end)
    let mut open: Option<(usize, usize, usize, usize)> = None;
    while i > 0 && j > 0 {
        let mut emit = false;
        if a[i - 1] == b[j - 1] {
            seq[k - 1] = a[i - 1];
            open = match open {
                None => Some((i - 1, i - 1, j - 1, j - 1)),
                Some((as_, ae, bs, be)) if as_ == i && bs == j => Some((i - 1, ae, j - 1, be)),
                o => {
                    emit = true;
                    o
                }
            };
            if open.is_some_and(|(as_, _, bs, _)| as_ == 0 || bs == 0) {
                emit = true;
            }
            k -= 1;
            i -= 1;
            j -= 1;
        } else {
            if t[(i - 1) * w + j] > t[i * w + j - 1] {
                i -= 1;
            } else {
                j -= 1;
            }
            emit = open.is_some();
        }
        // the run is closed whether or not it is long enough to report
        if emit
            && let Some((as_, ae, bs, be)) = open.take()
            && idx
            && (min == 0 || (ae - as_ + 1) as u64 >= min)
        {
            runs.push(Run { a: (as_, ae), b: (bs, be) });
        }
    }
    (seq, runs)
}

#[cfg(test)]
mod tests {
    use super::walk;

    #[test]
    fn the_runs_redis_reports_for_its_own_example() {
        let (seq, runs) = walk(b"ohmytext", b"mynewtext", true, 0);
        assert_eq!(seq, b"mytext");
        let got: Vec<_> = runs.iter().map(|r| (r.a, r.b)).collect();
        assert_eq!(got, [((4, 7), (5, 8)), ((2, 3), (0, 1))]);
        let (_, long) = walk(b"ohmytext", b"mynewtext", true, 4);
        assert_eq!(long.len(), 1);
    }

    #[test]
    fn nothing_in_common_is_empty() {
        let (seq, runs) = walk(b"abc", b"xyz", true, 0);
        assert!(seq.is_empty() && runs.is_empty());
        assert!(walk(b"", b"abc", true, 0).0.is_empty());
    }
}
