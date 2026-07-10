//! kevy-store zset baseline: zadd/zrange at 1k members (tree encoding) and
//! zadd around the inline↔tree encoding switch (small-zset inline holds at
//! most 2 short members; the 3rd promotes to the HashMap+BTreeSet form).

use crate::rng::Rng;
use kevy_bench::{bench, black_box};
use kevy_store::Store;

const KEY: &[u8] = b"stone:zset";

fn pairs_1k() -> Vec<(f64, Vec<u8>)> {
    let mut rng = Rng::new(0x5704_e5e5);
    (0..1000)
        .map(|i| (rng.f32_unit() as f64 * 1000.0, format!("member:{i:04}").into_bytes()))
        .collect()
}

fn bench_1k_key(pairs: &[(f64, Vec<u8>)]) {
    let s = bench(20, 1, || {
        let mut st = Store::new();
        for i in 0..pairs.len() {
            black_box(st.zadd(KEY, black_box(&pairs[i..=i])).unwrap());
        }
        black_box(&st);
    });
    crate::row("zadd 1k members, one key (tree)", s, pairs.len());

    let mut st = Store::new();
    st.zadd(KEY, pairs).unwrap();
    let s = bench(30, 20, || {
        black_box(st.zrange(KEY, 0, -1).unwrap());
    });
    crate::row("zrange 0 -1 (1k members)", s, 1);
}

/// zadd `members_per_key` short members into each of `nkeys` fresh keys.
/// 2/key stays on the inline encoding; 3/key crosses the promote point on
/// every third zadd, so that row averages the inline→tree switch in.
fn bench_switch(nkeys: usize, members_per_key: usize, label: &str) {
    let keys: Vec<Vec<u8>> = (0..nkeys).map(|i| format!("k:{i:04}").into_bytes()).collect();
    let members: [&[u8]; 3] = [b"m0", b"m1", b"m2"];
    let total = nkeys * members_per_key;
    let s = bench(20, 1, || {
        let mut st = Store::new();
        for k in &keys {
            for (j, m) in members.iter().take(members_per_key).enumerate() {
                black_box(st.zadd(k, &[(j as f64, m.to_vec())]).unwrap());
            }
        }
        black_box(&st);
    });
    crate::row(label, s, total);
}

pub fn run() {
    println!("== kevy-store (zset) ==");
    bench_1k_key(&pairs_1k());
    bench_switch(500, 2, "zadd 2 short members/key (stays inline)");
    bench_switch(334, 3, "zadd 3 members/key (inline→tree promote)");
    println!();
}
