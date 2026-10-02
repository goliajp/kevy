//! XXH3, 64-bit, seed 0 and the default secret: the digest Redis's
//! `DIGEST` answers and `SET … IFDEQ` compares against.

const PRIME32_1: u64 = 0x9E37_79B1;
const PRIME32_2: u64 = 0x85EB_CA77;
const PRIME32_3: u64 = 0xC2B2_AE3D;
const PRIME64_1: u64 = 0x9E37_79B1_85EB_CA87;
const PRIME64_2: u64 = 0xC2B2_AE3D_27D4_EB4F;
const PRIME64_3: u64 = 0x1656_67B1_9E37_79F9;
const PRIME64_4: u64 = 0x85EB_CA77_C2B2_AE63;
const PRIME64_5: u64 = 0x27D4_EB2F_1656_67C5;
const PRIME_MX1: u64 = 0x1656_6791_9E37_79F9;
const PRIME_MX2: u64 = 0x9FB2_1C65_1E98_DF25;

const SECRET: [u8; 192] = [
    0xb8, 0xfe, 0x6c, 0x39, 0x23, 0xa4, 0x4b, 0xbe, 0x7c, 0x01, 0x81, 0x2c, 0xf7, 0x21, 0xad, 0x1c,
    0xde, 0xd4, 0x6d, 0xe9, 0x83, 0x90, 0x97, 0xdb, 0x72, 0x40, 0xa4, 0xa4, 0xb7, 0xb3, 0x67, 0x1f,
    0xcb, 0x79, 0xe6, 0x4e, 0xcc, 0xc0, 0xe5, 0x78, 0x82, 0x5a, 0xd0, 0x7d, 0xcc, 0xff, 0x72, 0x21,
    0xb8, 0x08, 0x46, 0x74, 0xf7, 0x43, 0x24, 0x8e, 0xe0, 0x35, 0x90, 0xe6, 0x81, 0x3a, 0x26, 0x4c,
    0x3c, 0x28, 0x52, 0xbb, 0x91, 0xc3, 0x00, 0xcb, 0x88, 0xd0, 0x65, 0x8b, 0x1b, 0x53, 0x2e, 0xa3,
    0x71, 0x64, 0x48, 0x97, 0xa2, 0x0d, 0xf9, 0x4e, 0x38, 0x19, 0xef, 0x46, 0xa9, 0xde, 0xac, 0xd8,
    0xa8, 0xfa, 0x76, 0x3f, 0xe3, 0x9c, 0x34, 0x3f, 0xf9, 0xdc, 0xbb, 0xc7, 0xc7, 0x0b, 0x4f, 0x1d,
    0x8a, 0x51, 0xe0, 0x4b, 0xcd, 0xb4, 0x59, 0x31, 0xc8, 0x9f, 0x7e, 0xc9, 0xd9, 0x78, 0x73, 0x64,
    0xea, 0xc5, 0xac, 0x83, 0x34, 0xd3, 0xeb, 0xc3, 0xc5, 0x81, 0xa0, 0xff, 0xfa, 0x13, 0x63, 0xeb,
    0x17, 0x0d, 0xdd, 0x51, 0xb7, 0xf0, 0xda, 0x49, 0xd3, 0x16, 0x55, 0x26, 0x29, 0xd4, 0x68, 0x9e,
    0x2b, 0x16, 0xbe, 0x58, 0x7d, 0x47, 0xa1, 0xfc, 0x8f, 0xf8, 0xb8, 0xd1, 0x7a, 0xd0, 0x31, 0xce,
    0x45, 0xcb, 0x3a, 0x8f, 0x95, 0x16, 0x04, 0x28, 0xaf, 0xd7, 0xfb, 0xca, 0xbb, 0x4b, 0x40, 0x7e,
];

const STRIPE: usize = 64;
const STRIPES_PER_BLOCK: usize = (SECRET.len() - STRIPE) / 8;
const BLOCK: usize = STRIPE * STRIPES_PER_BLOCK;

/// The XXH3 64-bit hash of `data` (seed 0, default secret).
///
/// ```
/// assert_eq!(kevy_hash::xxh3_64(b""), 0x2d06_8005_38d3_94c2);
/// ```
pub fn xxh3_64(data: &[u8]) -> u64 {
    let len = data.len();
    match len {
        0 => avalanche64(r64(&SECRET, 56) ^ r64(&SECRET, 64)),
        1..=3 => len_1to3(data),
        4..=8 => len_4to8(data),
        9..=16 => len_9to16(data),
        17..=128 => len_17to128(data),
        129..=240 => len_129to240(data),
        _ => long(data),
    }
}

fn r64(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().expect("eight bytes"))
}

fn r32(b: &[u8], at: usize) -> u64 {
    u64::from(u32::from_le_bytes(b[at..at + 4].try_into().expect("four bytes")))
}

fn fold(a: u64, b: u64) -> u64 {
    let p = u128::from(a) * u128::from(b);
    p as u64 ^ (p >> 64) as u64
}

fn avalanche64(mut h: u64) -> u64 {
    h ^= h >> 33;
    h = h.wrapping_mul(PRIME64_2);
    h ^= h >> 29;
    h = h.wrapping_mul(PRIME64_3);
    h ^ (h >> 32)
}

fn avalanche(mut h: u64) -> u64 {
    h ^= h >> 37;
    h = h.wrapping_mul(PRIME_MX1);
    h ^ (h >> 32)
}

fn len_1to3(d: &[u8]) -> u64 {
    let len = d.len();
    let combined = (u64::from(d[0]) << 16)
        | (u64::from(d[len >> 1]) << 24)
        | u64::from(d[len - 1])
        | ((len as u64) << 8);
    avalanche64(combined ^ (r32(&SECRET, 0) ^ r32(&SECRET, 4)))
}

fn len_4to8(d: &[u8]) -> u64 {
    let len = d.len();
    let input = r32(d, len - 4).wrapping_add(r32(d, 0) << 32);
    let mut h = input ^ (r64(&SECRET, 8) ^ r64(&SECRET, 16));
    h ^= h.rotate_left(49) ^ h.rotate_left(24);
    h = h.wrapping_mul(PRIME_MX2);
    h ^= (h >> 35).wrapping_add(len as u64);
    h = h.wrapping_mul(PRIME_MX2);
    h ^ (h >> 28)
}

fn len_9to16(d: &[u8]) -> u64 {
    let len = d.len();
    let lo = r64(d, 0) ^ (r64(&SECRET, 24) ^ r64(&SECRET, 32));
    let hi = r64(d, len - 8) ^ (r64(&SECRET, 40) ^ r64(&SECRET, 48));
    let acc =
        (len as u64).wrapping_add(lo.swap_bytes()).wrapping_add(hi).wrapping_add(fold(lo, hi));
    avalanche(acc)
}

fn mix16(d: &[u8], at: usize, secret_at: usize) -> u64 {
    fold(r64(d, at) ^ r64(&SECRET, secret_at), r64(d, at + 8) ^ r64(&SECRET, secret_at + 8))
}

fn len_17to128(d: &[u8]) -> u64 {
    let len = d.len();
    let mut acc = (len as u64).wrapping_mul(PRIME64_1);
    // pairs from the outside in: (0, len-16), (16, len-32), …
    let pairs = (len - 1) / 32;
    for i in (0..=pairs).rev() {
        acc = acc.wrapping_add(mix16(d, 16 * i, 32 * i));
        acc = acc.wrapping_add(mix16(d, len - 16 * (i + 1), 32 * i + 16));
    }
    avalanche(acc)
}

fn len_129to240(d: &[u8]) -> u64 {
    let len = d.len();
    let mut acc = (len as u64).wrapping_mul(PRIME64_1);
    for i in 0..8 {
        acc = acc.wrapping_add(mix16(d, 16 * i, 16 * i));
    }
    let mut acc_end = mix16(d, len - 16, 136 - 17);
    acc = avalanche(acc);
    for i in 8..len / 16 {
        acc_end = acc_end.wrapping_add(mix16(d, 16 * i, 16 * (i - 8) + 3));
    }
    avalanche(acc.wrapping_add(acc_end))
}

fn accumulate_stripe(acc: &mut [u64; 8], d: &[u8], at: usize, secret_at: usize) {
    for i in 0..8 {
        let v = r64(d, at + 8 * i);
        let k = v ^ r64(&SECRET, secret_at + 8 * i);
        acc[i ^ 1] = acc[i ^ 1].wrapping_add(v);
        acc[i] = acc[i].wrapping_add((k & 0xFFFF_FFFF).wrapping_mul(k >> 32));
    }
}

fn scramble(acc: &mut [u64; 8]) {
    for (i, a) in acc.iter_mut().enumerate() {
        let mut v = *a;
        v ^= v >> 47;
        v ^= r64(&SECRET, SECRET.len() - STRIPE + 8 * i);
        *a = v.wrapping_mul(PRIME32_1);
    }
}

fn long(d: &[u8]) -> u64 {
    let len = d.len();
    let mut acc =
        [PRIME32_3, PRIME64_1, PRIME64_2, PRIME64_3, PRIME64_4, PRIME32_2, PRIME64_5, PRIME32_1];
    let blocks = (len - 1) / BLOCK;
    for b in 0..blocks {
        for s in 0..STRIPES_PER_BLOCK {
            accumulate_stripe(&mut acc, d, b * BLOCK + s * STRIPE, s * 8);
        }
        scramble(&mut acc);
    }
    let stripes = ((len - 1) - BLOCK * blocks) / STRIPE;
    for s in 0..stripes {
        accumulate_stripe(&mut acc, d, blocks * BLOCK + s * STRIPE, s * 8);
    }
    accumulate_stripe(&mut acc, d, len - STRIPE, SECRET.len() - STRIPE - 7);
    let mut h = (len as u64).wrapping_mul(PRIME64_1);
    for i in 0..4 {
        h = h.wrapping_add(fold(
            acc[2 * i] ^ r64(&SECRET, 11 + 16 * i),
            acc[2 * i + 1] ^ r64(&SECRET, 11 + 16 * i + 8),
        ));
    }
    avalanche(h)
}
