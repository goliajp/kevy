//! `HSCAN` / `SSCAN` / `ZSCAN key cursor [MATCH pattern] [COUNT n]
//! [NOVALUES]`: the store pages the collection; this reads the arguments
//! in the order Redis reads them — the cursor, then whether the key is
//! there at all (a missing one answers an empty page before any option is
//! looked at), then its type, then the options.

use kevy_resp::{ArgvView, encode_array_len, encode_bulk, encode_error};
use kevy_store::Store;

use crate::args::arg_i64;
use crate::reply::{ERR_NOT_INT, ERR_SYNTAX, WRONGTYPE, store_err, wrong_args};

/// Which collection a scan walks.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Hash,
    Set,
    ZSet,
}

impl Kind {
    fn names(self) -> (&'static str, &'static str) {
        match self {
            Kind::Hash => ("hscan", "hash"),
            Kind::Set => ("sscan", "set"),
            Kind::ZSet => ("zscan", "zset"),
        }
    }
}

struct Opts<'a> {
    pattern: Option<&'a [u8]>,
    count: usize,
    no_values: bool,
}

/// A cursor as Redis reads one: an unsigned decimal, a leading `+`, blanks
/// or `-0` allowed as `strtoull` allows them, a negative refused unless it
/// is out of `i64` range, where `strtoull`'s wrap-around applies.
fn cursor(raw: &[u8]) -> Option<u64> {
    if let Some(n) = arg_i64(raw) {
        return u64::try_from(n).ok();
    }
    let s = raw.trim_ascii_start();
    let (neg, digits) = match s.split_first() {
        Some((b'-', rest)) => (true, rest),
        Some((b'+', rest)) => (false, rest),
        _ => (false, s),
    };
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let v: u64 = std::str::from_utf8(digits).ok()?.parse().ok()?;
    Some(if neg { v.wrapping_neg() } else { v })
}

fn options<A: ArgvView + ?Sized>(args: &A, kind: Kind) -> Result<Opts<'_>, &'static str> {
    let mut o = Opts { pattern: None, count: 10, no_values: false };
    let mut i = 3;
    while i < args.len() {
        let word = &args[i];
        let has_value = i + 1 < args.len();
        if word.eq_ignore_ascii_case(b"COUNT") && has_value {
            let n = arg_i64(&args[i + 1]).ok_or(ERR_NOT_INT)?;
            o.count = usize::try_from(n).ok().filter(|&n| n >= 1).ok_or(ERR_SYNTAX)?;
            i += 2;
        } else if word.eq_ignore_ascii_case(b"MATCH") && has_value {
            // `*` matches everything, so it filters nothing
            o.pattern = Some(&args[i + 1]).filter(|p| p != b"*");
            i += 2;
        } else if word.eq_ignore_ascii_case(b"NOVALUES") {
            if kind != Kind::Hash {
                return Err("ERR NOVALUES option can only be used in HSCAN");
            }
            o.no_values = true;
            i += 1;
        } else {
            return Err(ERR_SYNTAX);
        }
    }
    Ok(o)
}

pub(crate) fn scan<A: ArgvView + ?Sized>(
    store: &mut Store,
    args: &A,
    kind: Kind,
    out: &mut Vec<u8>,
) {
    let (name, type_name) = kind.names();
    if args.len() < 3 {
        return wrong_args(out, name);
    }
    let Some(cursor) = cursor(&args[2]) else {
        return encode_error(out, "ERR invalid cursor");
    };
    match store.type_of(&args[1]) {
        "none" => return empty_page(out),
        t if t != type_name => return encode_error(out, WRONGTYPE),
        _ => {}
    }
    let o = match options(args, kind) {
        Ok(o) => o,
        Err(e) => return encode_error(out, e),
    };
    page(store, &args[1], cursor, kind, &o, out);
}

/// `["0", []]`: the page of a key that is not there.
fn empty_page(out: &mut Vec<u8>) {
    encode_array_len(out, 2);
    encode_bulk(out, b"0");
    encode_array_len(out, 0);
}

/// `[cursor, [item ...]]`: one page of `key`, its members filtered by
/// the pattern. The items go straight into `out`; the cursor and their
/// count, known only once the page is read, are put in front of them.
fn page(store: &mut Store, key: &[u8], cursor: u64, kind: Kind, o: &Opts<'_>, out: &mut Vec<u8>) {
    let keep = |m: &[u8]| o.pattern.is_none_or(|p| kevy_store::glob_match(p, m));
    let start = out.len();
    let mut n = 0;
    let next = match kind {
        Kind::Hash => store.hscan(key, cursor, o.count, |f, v| {
            if keep(f) {
                encode_bulk(out, f);
                n += 1;
                if !o.no_values {
                    encode_bulk(out, v);
                    n += 1;
                }
            }
        }),
        Kind::Set => store.sscan(key, cursor, o.count, |m| {
            if keep(m) {
                encode_bulk(out, m);
                n += 1;
            }
        }),
        Kind::ZSet => store.zscan(key, cursor, o.count, |m, sc| {
            if keep(m) {
                encode_bulk(out, m);
                kevy_resp::encode_bulk_double(out, sc);
                n += 2;
            }
        }),
    };
    let next = match next {
        Ok(next) => next,
        Err(e) => {
            out.truncate(start);
            return store_err(out, e);
        }
    };
    let body = out.len();
    encode_array_len(out, 2);
    crate::reply::encode_bulk_fmt(out, format_args!("{next}"));
    encode_array_len(out, n);
    let head = out.len() - body;
    out[start..].rotate_right(head);
}
