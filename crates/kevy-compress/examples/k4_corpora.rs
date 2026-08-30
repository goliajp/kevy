//! Our codec against the K4 premise table: the same four corpora the
//! zlib oracle measured (`bench/k4_premise.py`), run through
//! `kevy_compress::{train, encode}` — per-datum vs shared-dict, bytes
//! per value. The gap to the oracle's numbers is RFC §6.1's
//! "match-finder misses" term, measured instead of estimated.

fn pad(mut s: Vec<u8>, target: usize) -> Vec<u8> {
    while s.len() < target {
        s.push(b' ');
    }
    s.truncate(target);
    s
}

/// Deterministic xorshift so the corpora are reproducible without deps.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn pick<'a, T>(&mut self, xs: &'a [T]) -> &'a T {
        &xs[(self.next() % xs.len() as u64) as usize]
    }
}

const N: usize = 1000;
const TARGET: usize = 400;

fn corpus_identical() -> Vec<Vec<u8>> {
    let v = pad(
        format!(
            "{{\"id\": 123456, \"email\": \"ada@example.com\", \"status\": \"active\", \
             \"plan\": \"pro\", \"note\": \"{}\"}}",
            "x".repeat(260)
        )
        .into_bytes(),
        TARGET,
    );
    vec![v; N]
}

fn corpus_templated(rng: &mut Rng) -> Vec<Vec<u8>> {
    let statuses = ["active", "pending", "closed"];
    let plans = ["free", "pro", "team"];
    let alphabet: Vec<char> = "abcdefgh ".chars().collect();
    (0..N)
        .map(|i| {
            let note: String = (0..230).map(|_| *rng.pick(&alphabet)).collect();
            pad(
                format!(
                    "{{\"id\": {}, \"email\": \"user{i}@example{}.com\", \"status\": \"{}\", \
                     \"plan\": \"{}\", \"created_at\": {}, \"note\": \"{note}\"}}",
                    100_000 + i,
                    i % 7,
                    rng.pick(&statuses),
                    rng.pick(&plans),
                    1_700_000_000u64 + i as u64 * 37,
                )
                .into_bytes(),
                TARGET,
            )
        })
        .collect()
}

fn corpus_random(rng: &mut Rng) -> Vec<Vec<u8>> {
    (0..N).map(|_| (0..TARGET).map(|_| rng.next() as u8).collect()).collect()
}

fn corpus_textual(rng: &mut Rng) -> Vec<Vec<u8>> {
    let vocab: Vec<&str> = "the order was shipped to the warehouse and the invoice \
                            was settled by the customer account after review"
        .split_whitespace()
        .collect();
    (0..N)
        .map(|_| {
            let words: Vec<&str> = (0..70).map(|_| *rng.pick(&vocab)).collect();
            pad(words.join(" ").into_bytes(), TARGET)
        })
        .collect()
}

fn run(name: &str, values: &[Vec<u8>], oracle_pd: f64, oracle_dict: f64) {
    let per_datum: usize = values.iter().map(|v| kevy_compress::encode(&[], v).len()).sum();
    let refs: Vec<&[u8]> = values.iter().map(|v| v.as_slice()).collect();
    let dict = kevy_compress::train(&refs, kevy_compress::MAX_OFFSET);
    let with_dict: usize =
        dict.len() + values.iter().map(|v| kevy_compress::encode(&dict, v).len()).sum::<usize>();
    // Round-trip guard: the numbers only count if identity holds.
    for v in values.iter().take(20) {
        let f = kevy_compress::encode(&dict, v);
        assert_eq!(&kevy_compress::decode(&dict, &f).unwrap(), v);
    }
    let with_high: usize = dict.len()
        + values.iter().map(|v| kevy_compress::encode_high(&dict, v).len()).sum::<usize>();
    for v in values.iter().take(20) {
        let f = kevy_compress::encode_high(&dict, v);
        assert_eq!(&kevy_compress::decode(&dict, &f).unwrap(), v);
    }
    println!(
        "{name:<10} per-datum {:>6.1} B/val (oracle {oracle_pd:>6.1})   dict {:>6.1} (oracle {oracle_dict:>6.1})   dict+high {:>6.1}   [dict {} B counted]",
        per_datum as f64 / N as f64,
        with_dict as f64 / N as f64,
        with_high as f64 / N as f64,
        dict.len(),
    );
}

fn main() {
    let mut rng = Rng(0x2545_F491_4F6C_DD1D);
    println!("kevy-compress vs the zlib-oracle K4 table (N={N}, {TARGET} B values)");
    run("identical", &corpus_identical(), 89.0, 41.8);
    run("templated", &corpus_templated(&mut rng), 231.6, 180.0);
    run("random", &corpus_random(&mut rng), 411.0, 405.7);
    run("textual", &corpus_textual(&mut rng), 148.8, 103.8);
}
