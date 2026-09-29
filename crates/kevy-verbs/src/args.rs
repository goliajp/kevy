//! Reading argv tokens: verb case-folding, numbers, score bounds.
//!
//! ```
//! use kevy_verbs::args::{arg_i64, upper_verb};
//! let mut buf = [0u8; 32];
//! assert_eq!(upper_verb(b"hset", &mut buf), b"HSET");
//! // a value past i64 is not an integer, as Redis answers it
//! assert_eq!((arg_i64(b"-7"), arg_i64(b"9223372036854775808")), (Some(-7), None));
//! ```

use kevy_resp::ArgvView;
use kevy_store::ScoreBound;

/// Uppercase a verb into the caller's stack buffer, without a heap
/// allocation. A token longer than the buffer yields an empty slice,
/// which matches no verb, so it is answered as an unknown command.
///
/// ```
/// let mut buf = [0u8; 32];
/// assert_eq!(kevy_verbs::args::upper_verb(b"zAdd", &mut buf), b"ZADD");
/// assert!(kevy_verbs::args::upper_verb(&[b'x'; 33], &mut buf).is_empty());
/// ```
pub fn upper_verb<'a>(name: &[u8], buf: &'a mut [u8; 32]) -> &'a [u8] {
    let n = name.len();
    if n <= buf.len() {
        buf[..n].copy_from_slice(name);
        buf[..n].make_ascii_uppercase();
        &buf[..n]
    } else {
        &buf[..0]
    }
}

/// A decimal `i64`, or `None`.
///
/// ```
/// assert_eq!(kevy_verbs::args::arg_i64(b"-12"), Some(-12));
/// assert_eq!(kevy_verbs::args::arg_i64(b"1.5"), None);
/// ```
#[inline]
pub fn arg_i64(b: &[u8]) -> Option<i64> {
    std::str::from_utf8(b).ok()?.parse::<i64>().ok()
}

/// A decimal `u64`, or `None`.
///
/// ```
/// assert_eq!(kevy_verbs::args::arg_u64(b"7"), Some(7));
/// assert_eq!(kevy_verbs::args::arg_u64(b"-7"), None);
/// ```
#[inline]
pub fn arg_u64(b: &[u8]) -> Option<u64> {
    std::str::from_utf8(b).ok()?.parse::<u64>().ok()
}

/// A float, accepting the `inf` spellings and refusing NaN.
///
/// ```
/// use kevy_verbs::args::arg_f64;
/// assert_eq!(arg_f64(b"2.5"), Some(2.5));
/// assert_eq!(arg_f64(b"-inf"), Some(f64::NEG_INFINITY));
/// assert_eq!(arg_f64(b"nan"), None);
/// ```
pub fn arg_f64(b: &[u8]) -> Option<f64> {
    let s = std::str::from_utf8(b).ok()?.trim();
    let f: f64 = match s.to_ascii_lowercase().as_str() {
        "inf" | "+inf" | "infinity" | "+infinity" => f64::INFINITY,
        "-inf" | "-infinity" => f64::NEG_INFINITY,
        _ => s.parse().ok()?,
    };
    if f.is_nan() { None } else { Some(f) }
}

/// A score range bound: a leading `(` makes it exclusive.
///
/// ```
/// let b = kevy_verbs::args::parse_score_bound(b"(3").unwrap();
/// assert!(b.exclusive && b.value == 3.0);
/// ```
pub fn parse_score_bound(b: &[u8]) -> Option<ScoreBound> {
    match b.strip_prefix(b"(") {
        Some(rest) => Some(ScoreBound::exclusive(arg_f64(rest)?)),
        None => Some(ScoreBound::inclusive(arg_f64(b)?)),
    }
}

/// `args[from..]` as borrowed slices, without copying any argument.
///
/// ```
/// let argv = kevy_resp::Argv::from(vec![b"SADD".to_vec(), b"s".to_vec(), b"a".to_vec()]);
/// assert_eq!(kevy_verbs::args::rest_borrowed(&argv, 2), vec![&b"a"[..]]);
/// ```
pub fn rest_borrowed<A: ArgvView + ?Sized>(args: &A, from: usize) -> Vec<&[u8]> {
    (from..args.len()).map(|i| &args[i]).collect()
}

/// The `[MATCH pattern] [COUNT n]` tail of `HSCAN` / `SSCAN` / `ZSCAN`
/// from `start` on. COUNT is checked and then ignored: these scans
/// answer in one batch. `None` = a syntax error.
pub(crate) fn scan_match<A: ArgvView + ?Sized>(args: &A, start: usize) -> Option<Option<Vec<u8>>> {
    let mut pat = None;
    let mut i = start;
    while i < args.len() {
        let tok = &args[i];
        let val = args.get(i + 1)?;
        if tok.eq_ignore_ascii_case(b"MATCH") {
            pat = Some(val.to_vec());
        } else if tok.eq_ignore_ascii_case(b"COUNT") {
            arg_i64(val)?;
        } else {
            return None;
        }
        i += 2;
    }
    Some(pat)
}

/// The options of `SCAN cursor [MATCH pattern] [COUNT count] [TYPE type]`.
///
/// ```
/// let argv = kevy_resp::Argv::from(vec![b"SCAN".to_vec(), b"7".to_vec(), b"MATCH".to_vec(), b"a*".to_vec()]);
/// let o = kevy_verbs::args::scan_opts(&argv).unwrap();
/// assert_eq!(o.pattern.as_deref(), Some(&b"a*"[..]));
/// assert_eq!(o.count, 10);
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanOpts {
    /// Where to resume.
    pub cursor: u64,
    /// The work bound per call; Redis's default is 10.
    pub count: usize,
    /// A glob over key names.
    pub pattern: Option<Vec<u8>>,
    /// Keep only keys of this type (`string`, `hash`, …).
    pub type_filter: Option<Vec<u8>>,
}

/// Parse `SCAN`'s argv. `Err` carries the refusal in Redis's words.
///
/// ```
/// let argv = kevy_resp::Argv::from(vec![b"SCAN".to_vec(), b"0".to_vec(), b"COUNT".to_vec(), b"5".to_vec()]);
/// let o = kevy_verbs::args::scan_opts(&argv).unwrap();
/// assert_eq!((o.cursor, o.count), (0, 5));
/// ```
pub fn scan_opts<A: ArgvView + ?Sized>(args: &A) -> Result<ScanOpts, &'static str> {
    let cursor = args.get(1).and_then(arg_u64).ok_or("ERR invalid cursor")?;
    let mut opts = ScanOpts { cursor, count: 10, pattern: None, type_filter: None };
    let mut i = 2;
    while i < args.len() {
        let opt = &args[i];
        let Some(val) = args.get(i + 1) else {
            return Err("ERR syntax error");
        };
        if opt.eq_ignore_ascii_case(b"MATCH") {
            opts.pattern = Some(val.to_vec());
        } else if opt.eq_ignore_ascii_case(b"COUNT") {
            let n = arg_i64(val).ok_or("ERR value is not an integer or out of range")?;
            if n < 1 {
                return Err("ERR syntax error");
            }
            opts.count = n as usize;
        } else if opt.eq_ignore_ascii_case(b"TYPE") {
            opts.type_filter = Some(val.to_vec());
        } else {
            return Err("ERR syntax error");
        }
        i += 2;
    }
    Ok(opts)
}
