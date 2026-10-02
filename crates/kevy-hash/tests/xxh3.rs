//! XXH3 against the digests Redis 8.10.2's `DIGEST` gave for the same
//! bytes: every length through the short, mid-size and long paths, and
//! the long path's block and stripe boundaries.

/// The first `n` bytes of a fixed pseudo-random stream; the vectors were
/// taken over prefixes of this same stream.
fn stream(n: usize) -> Vec<u8> {
    let mut x: u64 = 0x853c_49e6_748f_ea9b;
    (0..n)
        .map(|_| {
            x = x.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
            (x >> 56) as u8
        })
        .collect()
}

#[test]
fn matches_redis_digest_at_every_length_class() {
    let table = include_str!("xxh3_redis.txt");
    let data = stream(10_000);
    let mut checked = 0;
    for line in table.lines() {
        let (n, hex) = line.split_once(' ').unwrap();
        let n: usize = n.parse().unwrap();
        let want = u64::from_str_radix(hex, 16).unwrap();
        assert_eq!(kevy_hash::xxh3_64(&data[..n]), want, "length {n}");
        checked += 1;
    }
    assert_eq!(checked, 277);
}
