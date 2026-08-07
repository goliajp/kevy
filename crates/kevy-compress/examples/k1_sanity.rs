// K1 sanity: decode throughput on a 4 KiB structured value, this box.
fn main() {
    let mut text = Vec::new();
    for i in 0..80 {
        text.extend_from_slice(
            format!("{{\"user\":\"u{i}\",\"role\":\"admin\",\"active\":true,\"path\":\"/api/v2/items/{i}\"}}\n").as_bytes());
    }
    text.truncate(4096);
    let dict = kevy_compress::train(&[&text], 65535);
    let frame = kevy_compress::encode(&dict, &text);
    println!("4KiB -> {} frame bytes (ratio {:.2}x)", frame.len(), text.len() as f64 / frame.len() as f64);
    let n = 200_000;
    let t0 = std::time::Instant::now();
    let mut sink = 0usize;
    for _ in 0..n {
        let out = kevy_compress::decode(&dict, &frame).unwrap();
        sink = sink.wrapping_add(out.len() + out[0] as usize);
    }
    let el = t0.elapsed().as_secs_f64();
    println!("decode: {:.2} GB/s ({} iters, sink {})", (n as f64 * text.len() as f64) / el / 1e9, n, sink);
}
