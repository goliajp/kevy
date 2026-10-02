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

/// Append `v` as a bulk string of [`write_double`]'s text, as a RESP2
/// reply carries a score, written in place (see
/// [`crate::encode_bulk_with`]).
///
/// ```
/// let mut out = Vec::new();
/// kevy_resp::encode_bulk_double(&mut out, 2.5);
/// kevy_resp::encode_bulk_double(&mut out, -1.2345e-17);
/// assert_eq!(out, b"$3\r\n2.5\r\n$11\r\n-1.2345e-17\r\n");
/// ```
pub fn encode_bulk_double(out: &mut Vec<u8>, v: f64) {
    crate::encode_bulk_with(out, |o| write_double(o, v));
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

#[cfg(test)]
mod tests {
    use super::{encode_bulk_double, write_double};

    #[test]
    fn a_bulk_double_is_the_bulk_of_the_text() {
        let mut x = 0x9e37_79b9_7f4a_7c15u64;
        let mut values =
            vec![0.0, -0.0, 1.0, 123_456_789.0, 1_234_567_890.0, 0.1, 1e-300, f64::INFINITY];
        for _ in 0..20_000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            values.push(f64::from_bits(x));
            values.push((x % 10_000_000_000) as f64 / 7.0);
        }
        for v in values {
            let mut text = Vec::new();
            write_double(&mut text, v);
            let mut want = format!("${}\r\n", text.len()).into_bytes();
            want.extend_from_slice(&text);
            want.extend_from_slice(b"\r\n");
            let mut got = b"prefix".to_vec();
            encode_bulk_double(&mut got, v);
            assert_eq!(&got[6..], &want[..], "{v:e}");
        }
    }
}
