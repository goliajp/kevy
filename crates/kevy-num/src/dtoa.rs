//! The digits of the Grisu2 algorithm (Loitsch, "Printing Floating-Point
//! Numbers Quickly and Accurately with Integers"), which always read back
//! to the same double but are not always the shortest such digits, laid
//! out as an integer, a plain decimal or scientific notation by the
//! digits' count and exponent.

use alloc::vec::Vec;

/// `10^k` for `k = -348, -340, …, 340`: a 64-bit significand rounded to
/// nearest, and its binary exponent.
const POWERS: [(u64, i32); 87] = [
    (0xfa8fd5a0081c0288, -1220),
    (0xbaaee17fa23ebf76, -1193),
    (0x8b16fb203055ac76, -1166),
    (0xcf42894a5dce35ea, -1140),
    (0x9a6bb0aa55653b2d, -1113),
    (0xe61acf033d1a45df, -1087),
    (0xab70fe17c79ac6ca, -1060),
    (0xff77b1fcbebcdc4f, -1034),
    (0xbe5691ef416bd60c, -1007),
    (0x8dd01fad907ffc3c, -980),
    (0xd3515c2831559a83, -954),
    (0x9d71ac8fada6c9b5, -927),
    (0xea9c227723ee8bcb, -901),
    (0xaecc49914078536d, -874),
    (0x823c12795db6ce57, -847),
    (0xc21094364dfb5637, -821),
    (0x9096ea6f3848984f, -794),
    (0xd77485cb25823ac7, -768),
    (0xa086cfcd97bf97f4, -741),
    (0xef340a98172aace5, -715),
    (0xb23867fb2a35b28e, -688),
    (0x84c8d4dfd2c63f3b, -661),
    (0xc5dd44271ad3cdba, -635),
    (0x936b9fcebb25c996, -608),
    (0xdbac6c247d62a584, -582),
    (0xa3ab66580d5fdaf6, -555),
    (0xf3e2f893dec3f126, -529),
    (0xb5b5ada8aaff80b8, -502),
    (0x87625f056c7c4a8b, -475),
    (0xc9bcff6034c13053, -449),
    (0x964e858c91ba2655, -422),
    (0xdff9772470297ebd, -396),
    (0xa6dfbd9fb8e5b88f, -369),
    (0xf8a95fcf88747d94, -343),
    (0xb94470938fa89bcf, -316),
    (0x8a08f0f8bf0f156b, -289),
    (0xcdb02555653131b6, -263),
    (0x993fe2c6d07b7fac, -236),
    (0xe45c10c42a2b3b06, -210),
    (0xaa242499697392d3, -183),
    (0xfd87b5f28300ca0e, -157),
    (0xbce5086492111aeb, -130),
    (0x8cbccc096f5088cc, -103),
    (0xd1b71758e219652c, -77),
    (0x9c40000000000000, -50),
    (0xe8d4a51000000000, -24),
    (0xad78ebc5ac620000, 3),
    (0x813f3978f8940984, 30),
    (0xc097ce7bc90715b3, 56),
    (0x8f7e32ce7bea5c70, 83),
    (0xd5d238a4abe98068, 109),
    (0x9f4f2726179a2245, 136),
    (0xed63a231d4c4fb27, 162),
    (0xb0de65388cc8ada8, 189),
    (0x83c7088e1aab65db, 216),
    (0xc45d1df942711d9a, 242),
    (0x924d692ca61be758, 269),
    (0xda01ee641a708dea, 295),
    (0xa26da3999aef774a, 322),
    (0xf209787bb47d6b85, 348),
    (0xb454e4a179dd1877, 375),
    (0x865b86925b9bc5c2, 402),
    (0xc83553c5c8965d3d, 428),
    (0x952ab45cfa97a0b3, 455),
    (0xde469fbd99a05fe3, 481),
    (0xa59bc234db398c25, 508),
    (0xf6c69a72a3989f5c, 534),
    (0xb7dcbf5354e9bece, 561),
    (0x88fcf317f22241e2, 588),
    (0xcc20ce9bd35c78a5, 614),
    (0x98165af37b2153df, 641),
    (0xe2a0b5dc971f303a, 667),
    (0xa8d9d1535ce3b396, 694),
    (0xfb9b7cd9a4a7443c, 720),
    (0xbb764c4ca7a44410, 747),
    (0x8bab8eefb6409c1a, 774),
    (0xd01fef10a657842c, 800),
    (0x9b10a4e5e9913129, 827),
    (0xe7109bfba19c0c9d, 853),
    (0xac2820d9623bf429, 880),
    (0x80444b5e7aa7cf85, 907),
    (0xbf21e44003acdd2d, 933),
    (0x8e679c2f5e44ff8f, 960),
    (0xd433179d9c8cb841, 986),
    (0x9e19db92b4e31ba9, 1013),
    (0xeb96bf6ebadf77d9, 1039),
    (0xaf87023b9bf0ee6b, 1066),
];
const FIRST_POWER: i32 = -348;
const POWER_STEP: i32 = 8;
/// The scaled exponent is brought to at least this; the step between
/// cached powers is narrower than the window up to -32, so the lowest
/// power that reaches it also stays inside.
const ALPHA: i32 = -60;

#[derive(Clone, Copy)]
struct Fp {
    f: u64,
    e: i32,
}

impl Fp {
    fn normalized(self) -> Fp {
        let s = self.f.leading_zeros();
        Fp { f: self.f << s, e: self.e - s as i32 }
    }

    /// The product's upper 64 bits, rounded.
    fn mul(self, o: Fp) -> Fp {
        let p = u128::from(self.f) * u128::from(o.f);
        Fp { f: ((p + (1 << 63)) >> 64) as u64, e: self.e + o.e + 64 }
    }
}

/// Append `v` as the fpconv library prints it: the Grisu2 digits as an
/// integer, a plain decimal or scientific notation, chosen by the digits'
/// count and exponent; `0`, `inf`, `-inf` and `nan` as themselves.
///
/// ```
/// let mut out = Vec::new();
/// for v in [3.0, 0.0001, 0.00012345, 1e20, 0.1, -2.5e-7, 8.07e29] {
///     kevy_num::write_grisu2(&mut out, v);
///     out.push(b' ');
/// }
/// assert_eq!(out, b"3 0.0001 1.2345e-4 1e+20 0.1 -2.5e-7 8.069999999999999e+29 ");
/// ```
pub fn write_grisu2(out: &mut Vec<u8>, v: f64) {
    if v.is_nan() {
        return out.extend_from_slice(b"nan");
    }
    if v.is_infinite() {
        return out.extend_from_slice(if v > 0.0 { b"inf" } else { b"-inf" });
    }
    if v == 0.0 {
        return out.extend_from_slice(if v.is_sign_negative() { b"-0" } else { b"0" });
    }
    let mut digits = [0u8; 20];
    let (mut n, mut k) = grisu2(v.abs(), &mut digits);
    while n > 1 && digits[n - 1] == b'0' {
        n -= 1;
        k += 1;
    }
    if v < 0.0 {
        out.push(b'-');
    }
    lay_out(out, &digits[..n], k);
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

/// `digits × 10^k` as an integer, a plain decimal or scientific notation.
fn lay_out(out: &mut Vec<u8>, d: &[u8], k: i32) {
    let n = d.len() as i32;
    let exp = k + n - 1;
    if k >= 0 && exp < n + 7 {
        out.extend_from_slice(d);
        out.resize(out.len() + k as usize, b'0');
    } else if k < 0 && (k > -7 || exp.abs() < 4) {
        let point = n + k;
        if point <= 0 {
            out.extend_from_slice(b"0.");
            out.resize(out.len() + (-point) as usize, b'0');
            out.extend_from_slice(d);
        } else {
            out.extend_from_slice(&d[..point as usize]);
            out.push(b'.');
            out.extend_from_slice(&d[point as usize..]);
        }
    } else {
        out.push(d[0]);
        if n > 1 {
            out.push(b'.');
            out.extend_from_slice(&d[1..]);
        }
        out.extend_from_slice(if exp < 0 { b"e-" } else { b"e+" });
        write_int(out, i64::from(exp.abs()));
    }
}

/// The digits of a positive finite `v` into `buf`: their count and the
/// power of ten of the last one.
fn grisu2(v: f64, buf: &mut [u8; 20]) -> (usize, i32) {
    let bits = v.to_bits();
    let (f, e) = match (bits >> 52) as i32 {
        0 => (bits & ((1 << 52) - 1), -1074),
        ex => ((bits & ((1 << 52) - 1)) | (1 << 52), ex - 1075),
    };
    let w = Fp { f, e }.normalized();
    let upper = Fp { f: (f << 1) + 1, e: e - 1 }.normalized();
    // the gap below a power of two is half the gap above it
    let lower = if f == 1 << 52 && e > -1074 {
        Fp { f: (f << 2) - 1, e: e - 2 }
    } else {
        Fp { f: (f << 1) - 1, e: e - 1 }
    };
    let lower = Fp { f: lower.f << (lower.e - upper.e), e: upper.e };
    let (c, k) = cached_power(w.e);
    let scaled = w.mul(c);
    let mut hi = upper.mul(c);
    let mut lo = lower.mul(c);
    hi.f -= 1;
    lo.f += 1;
    generate(scaled, hi, hi.f - lo.f, -k, buf)
}

/// The lowest cached power that brings `e + 64` up to `ALPHA`.
fn cached_power(e: i32) -> (Fp, i32) {
    let fits = |i: usize| POWERS[i].1 + e + 64 >= ALPHA;
    // the decimal power whose binary exponent is about the one wanted
    let k = f64::from(ALPHA - e - 1) * core::f64::consts::LOG10_2;
    let mut i = ((k as i32 - FIRST_POWER) / POWER_STEP).clamp(0, POWERS.len() as i32 - 1) as usize;
    while !fits(i) {
        i += 1;
    }
    while i > 0 && fits(i - 1) {
        i -= 1;
    }
    let (f, pe) = POWERS[i];
    (Fp { f, e: pe }, FIRST_POWER + POWER_STEP * i as i32)
}

/// Digits of `hi` until what is left of it is within `delta` of the true
/// value's neighbourhood, then the last digit nudged toward `w`.
fn generate(w: Fp, hi: Fp, mut delta: u64, mut k: i32, buf: &mut [u8; 20]) -> (usize, i32) {
    let shift = -hi.e as u32;
    let one = 1u64 << shift;
    let to_w = hi.f - w.f;
    let mut p1 = (hi.f >> shift) as u32;
    let mut p2 = hi.f & (one - 1);
    let mut n = 0;
    let mut kappa = if p1 == 0 { 0 } else { p1.ilog10() as i32 + 1 };
    let mut div = 10u32.pow(kappa.max(1) as u32 - 1);
    while kappa > 0 {
        let d = p1 / div;
        if d != 0 || n != 0 {
            buf[n] = b'0' + d as u8;
            n += 1;
        }
        p1 %= div;
        kappa -= 1;
        let rest = (u64::from(p1) << shift) + p2;
        if rest <= delta {
            weed(&mut buf[..n], delta.into(), rest.into(), u128::from(div) << shift, to_w.into());
            return (n, k + kappa);
        }
        div /= 10;
    }
    let mut unit = 1u128;
    loop {
        p2 *= 10;
        delta *= 10;
        unit *= 10;
        let d = p2 >> shift;
        if d != 0 || n != 0 {
            buf[n] = b'0' + d as u8;
            n += 1;
        }
        p2 &= one - 1;
        kappa -= 1;
        if p2 < delta {
            k += kappa;
            weed(&mut buf[..n], delta.into(), p2.into(), one.into(), u128::from(to_w) * unit);
            return (n, k);
        }
    }
}

/// Lower the last digit while that brings the printed value nearer the
/// true one and keeps it inside the neighbourhood.
fn weed(digits: &mut [u8], delta: u128, mut rest: u128, ten_kappa: u128, to_w: u128) {
    let last = digits.len() - 1;
    while rest < to_w
        && delta - rest >= ten_kappa
        && (rest + ten_kappa < to_w || to_w - rest > rest + ten_kappa - to_w)
    {
        digits[last] -= 1;
        rest += ten_kappa;
    }
}
