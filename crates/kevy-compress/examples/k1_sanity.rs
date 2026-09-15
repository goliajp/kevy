//! K1 sanity: the round trip works and a 4 KiB value shrinks. **Not a
//! throughput measurement**, though it prints one.
//!
//! The dictionary here is trained on the very value that is then
//! compressed, so the 4 KiB input becomes a ~100-byte frame — a 41x
//! ratio — and what the loop below times is one long match copied out of
//! the dictionary. No stored value is ever its own training sample, so
//! the GB/s figure describes nothing a read does. It was once quoted as
//! evidence that decode ran "an order of magnitude above" the crate's
//! 1 GB/s floor.
//!
//! For the budget, use `examples/decode_budget`, which holds values out
//! of the dictionary's training set.
fn main() {
    let mut text = Vec::new();
    for i in 0..80 {
        text.extend_from_slice(
            format!("{{\"user\":\"u{i}\",\"role\":\"admin\",\"active\":true,\"path\":\"/api/v2/items/{i}\"}}\n").as_bytes());
    }
    text.truncate(4096);
    let dict = kevy_compress::train(&[&text], 65535);
    let frame = kevy_compress::encode(&dict, &text);
    println!(
        "4KiB -> {} frame bytes (ratio {:.2}x)",
        frame.len(),
        text.len() as f64 / frame.len() as f64
    );
    let n = 200_000;
    let t0 = std::time::Instant::now();
    let mut sink = 0usize;
    for _ in 0..n {
        let out = kevy_compress::decode(&dict, &frame).unwrap();
        sink = sink.wrapping_add(out.len() + out[0] as usize);
    }
    let el = t0.elapsed().as_secs_f64();
    println!(
        "decode: {:.2} GB/s ({} iters, sink {})",
        (n as f64 * text.len() as f64) / el / 1e9,
        n,
        sink
    );
}
