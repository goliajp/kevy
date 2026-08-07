// K1 decode-budget probe (research instrument, not product code).
// Measures, on the target box, what "memcpy-class" actually means in
// µs per 4 KiB value, and what a naive LZ decode loop costs beside it.
use std::time::Instant;

const VAL: usize = 4096;
const REPS: usize = 200_000;

fn bench<F: FnMut(usize) -> u64>(name: &str, mut f: F) {
    // Warmup
    for i in 0..1000 { std::hint::black_box(f(i)); }
    let t = Instant::now();
    let mut acc = 0u64;
    for i in 0..REPS { acc = acc.wrapping_add(f(i)); }
    let el = t.elapsed();
    let us_per = el.as_secs_f64() * 1e6 / REPS as f64;
    let gbps = (VAL * REPS) as f64 / el.as_secs_f64() / 1e9;
    println!("{name:<28} {us_per:8.3} us/4KiB   {gbps:6.2} GB/s   (acc {acc})");
}

fn main() {
    // Source pool larger than L2 so copies aren't trivially cache-resident.
    let pool = vec![0xA5u8; 64 * 1024 * 1024];
    let mut dst = vec![0u8; VAL];

    bench("memcpy 4KiB", |i| {
        let off = (i * 8192) % (pool.len() - VAL);
        dst.copy_from_slice(&pool[off..off + VAL]);
        dst[0] as u64
    });

    // A naive LZ decode: alternating 16 B literal runs and 32 B
    // back-references at distance 64, wildcopy by 8-byte words —
    // the token mix a fast level would emit on templated rows.
    let lit = vec![0x5Au8; VAL];
    bench("naive LZ decode 4KiB", |i| {
        let mut out = 0usize;
        let mut lp = (i * 64) % (lit.len() - 32);
        while out + 48 <= VAL {
            dst.copy_within(out.saturating_sub(64)..out.saturating_sub(64) + 32, out);
            out += 32;
            dst[out..out + 16].copy_from_slice(&lit[lp..lp + 16]);
            out += 16;
            lp = (lp + 16) % (lit.len() - 32);
        }
        dst[VAL - 1] as u64
    });

    // Byte-at-a-time decode — the slow shape spg's lzss has (the 100
    // MiB/s the RFC warns about).
    bench("byte-loop decode 4KiB", |i| {
        let mut o = 1usize;
        let seed = (i & 0xff) as u8;
        dst[0] = seed;
        while o < VAL {
            dst[o] = dst[o - 1].wrapping_add(1);
            o += 1;
        }
        dst[VAL - 1] as u64
    });
}
