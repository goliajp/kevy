//! Reading argv tokens: verb case-folding, numbers, score bounds.

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
        Some(rest) => Some(ScoreBound { value: arg_f64(rest)?, exclusive: true }),
        None => Some(ScoreBound { value: arg_f64(b)?, exclusive: false }),
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
