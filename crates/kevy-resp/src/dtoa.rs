//! Doubles in Redis's reply form: an integral value of magnitude up to
//! 2^62 prints as that integer, anything else as [`kevy_num::write_grisu2`]
//! prints it.

const TWO_62: f64 = (1u64 << 62) as f64;

/// Append `v` as Redis prints a double reply's text.
///
/// ```
/// let mut out = Vec::new();
/// for v in [3.0, -0.0, 0.00012345, 1e20, 4611686018427387904.0, 4611686018427388928.0] {
///     kevy_resp::write_double(&mut out, v);
///     out.push(b' ');
/// }
/// assert_eq!(out, b"3 0 1.2345e-4 1e+20 4611686018427387904 4611686018427389000 ");
/// ```
pub fn write_double(out: &mut Vec<u8>, v: f64) {
    // exact comparison on purpose: only an integral value takes this form
    #[allow(clippy::float_cmp)]
    if v == v.trunc() && v.abs() <= TWO_62 {
        return write_int(out, v as i64);
    }
    kevy_num::write_grisu2(out, v);
}

fn write_int(out: &mut Vec<u8>, n: i64) {
    let mut tmp = [0u8; 20];
    let mut i = tmp.len();
    let mut m = n.unsigned_abs();
    loop {
        i -= 1;
        tmp[i] = b'0' + (m % 10) as u8;
        m /= 10;
        if m == 0 {
            break;
        }
    }
    if n < 0 {
        out.push(b'-');
    }
    out.extend_from_slice(&tmp[i..]);
}
