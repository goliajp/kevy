//! Does this crate meet the decode budget it states?
//!
//! `lib.rs` opens with "**Decode ≥ ~1 GB/s**: Speed is a requirement of
//! the design, not a later optimisation", and `decode.rs` says the probe
//! put it "an order of magnitude above" that floor. `k1_sanity` reports
//! 11 GB/s and is where that came from.
//!
//! It reports that by **training the dictionary on the very value it
//! then compresses**: the 4 KiB input becomes a 100-byte frame, a ratio
//! of 41×, because the value is a single long match into the dictionary.
//! The decode is a 4 KiB memcpy out of the dictionary, and the GB/s is
//! computed over the 4 KiB. No stored value is ever its own training
//! sample, so the number does not describe a read.
//!
//! This measures the shape `kevy-vlog` actually produces: a dictionary
//! trained on a sample of earlier values (`kevy-vlog/src/lib.rs:220`
//! calls `train(&refs, MAX_OFFSET)` at rotation), then values that were
//! NOT in that sample. It reports the ratio beside every throughput, so
//! a number lifted by a value that happens to be in the dictionary is
//! visible rather than flattering.
//!
//! Both paths are reported: `encode` is what a write takes, `encode_high`
//! is what compaction rewrites into, and a cold read decodes whichever
//! is on disk.

fn corpus(n: usize, seed: u64) -> Vec<Vec<u8>> {
    // Templated JSON, the shape the crate's own docs use as its example
    // workload. A deterministic LCG so a run is reproducible.
    let mut s = seed;
    let mut next = move || {
        s = s.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
        s >> 33
    };
    (0..n)
        .map(|_| {
            let (u, it, ms) = (next() % 100_000, next() % 9_999, next() % 5_000);
            format!(
                "{{\"user\":\"u{u}\",\"role\":\"admin\",\"active\":true,\
                 \"path\":\"/api/v2/items/{it}\",\"took_ms\":{ms},\
                 \"trace\":\"{:016x}\"}}",
                next()
            )
            .into_bytes()
        })
        .collect()
}

fn time<T>(iters: usize, mut f: impl FnMut() -> T) -> f64 {
    for _ in 0..iters / 10 {
        std::hint::black_box(f());
    }
    let t = std::time::Instant::now();
    for _ in 0..iters {
        std::hint::black_box(f());
    }
    t.elapsed().as_secs_f64() / iters as f64
}

fn main() {
    let train_set = corpus(4_000, 1);
    let refs: Vec<&[u8]> = train_set.iter().map(Vec::as_slice).collect();
    let dict = kevy_compress::train(&refs, kevy_compress::MAX_OFFSET);
    // Held out: a different seed, so no value here was a training sample.
    let held = corpus(200, 999);

    println!("dictionary {} B, trained on {} values", dict.len(), train_set.len());
    println!(
        "held-out values: {}, mean {} B\n",
        held.len(),
        held.iter().map(Vec::len).sum::<usize>() / held.len()
    );

    for (name, enc) in [
        ("encode      (write path)", kevy_compress::encode as fn(&[u8], &[u8]) -> Vec<u8>),
        ("encode_high (compaction)", kevy_compress::encode_high),
    ] {
        let frames: Vec<Vec<u8>> = held.iter().map(|v| enc(&dict, v)).collect();
        let orig: usize = held.iter().map(Vec::len).sum();
        let comp: usize = frames.iter().map(Vec::len).sum();

        let mut i = 0;
        let dt = time(20_000, || {
            i = (i + 1) % frames.len();
            kevy_compress::decode(&dict, &frames[i]).expect("round trip")
        });
        // The same frames through a dictionary parsed once, which is what
        // a per-file `Dict` gives a reader.
        let parsed = kevy_compress::Dict::new(&dict);
        let mut k = 0;
        let dt_held = time(20_000, || {
            k = (k + 1) % frames.len();
            kevy_compress::decode_with(&parsed, &frames[k]).expect("round trip")
        });
        let mean_len = orig as f64 / held.len() as f64;
        let gbps = mean_len / dt / 1e9;

        let mut j = 0;
        let et = time(2_000, || {
            j = (j + 1) % held.len();
            enc(&dict, &held[j])
        });
        let enc_with: fn(&kevy_compress::Dict, &[u8]) -> Vec<u8> =
            if name.starts_with("encode ") { kevy_compress::encode_with } else { kevy_compress::encode_high_with };
        let mut m = 0;
        let et_held = time(2_000, || {
            m = (m + 1) % held.len();
            enc_with(&parsed, &held[m])
        });
        // Same frames, or the change moved more than where the work is.
        for (v, f) in held.iter().zip(&frames) {
            assert_eq!(&enc_with(&parsed, v), f, "a Dict must produce the frame the slice does");
        }

        println!("{name}");
        println!("  ratio     {:.2}x  ({comp} B from {orig} B)", orig as f64 / comp as f64);
        println!("  decode    {:.3} GB/s   ({:.3} us/value)   [dict per call]", gbps, dt * 1e6);
        println!(
            "  decode    {:.3} GB/s   ({:.3} us/value)   [Dict parsed once]",
            mean_len / dt_held / 1e9,
            dt_held * 1e6
        );
        println!("  encode    {:.3} GB/s   ({:.3} us/value)   [dict per call]", mean_len / et / 1e9, et * 1e6);
        println!(
            "  encode    {:.3} GB/s   ({:.3} us/value)   [Dict seeded once]",
            mean_len / et_held / 1e9,
            et_held * 1e6
        );
        println!(
            "  budget    {}  (lib.rs states >= ~1 GB/s decode)\n",
            if mean_len / dt_held / 1e9 >= 1.0 { "MET" } else { "MISSED" }
        );
    }
}
