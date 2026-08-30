//! kevy-text baseline: tokenize a mixed CJK+ASCII document, build a bigram
//! inverted segment over 10k fixed-seed docs, and query it.

use crate::rng::Rng;
use kevy_bench::{bench, black_box};
use kevy_text::{TextSegment, tokenize};

const ASCII_WORDS: &[&str] = &[
    "kevy",
    "cache",
    "shard",
    "vector",
    "index",
    "query",
    "latency",
    "throughput",
    "reactor",
    "socket",
    "bench",
    "median",
    "stone",
    "steel",
    "cement",
    "tokyo",
    "osaka",
    "release",
];

// Real CJK characters so bigram segmentation sees production-shaped input.
const CJK_POOL: &[char] = &[
    '分', '布', '式', '缓', '存', '系', '统', '高', '性', '能', '数', '据', '库', '索', '引', '查',
    '询', '延', '迟', '吞', '吐', '量', '内', '存', '网', '络', '并', '发', '压', '缩',
];

/// One mixed doc: a handful of ASCII words interleaved with CJK runs; every
/// 10th doc carries the fixed phrase the query below looks for.
fn gen_doc(rng: &mut Rng, i: usize) -> Vec<u8> {
    let mut doc = String::new();
    for _ in 0..6 {
        doc.push_str(ASCII_WORDS[rng.below(ASCII_WORDS.len())]);
        doc.push(' ');
        for _ in 0..4 {
            doc.push(CJK_POOL[rng.below(CJK_POOL.len())]);
        }
        doc.push(' ');
    }
    if i.is_multiple_of(10) {
        doc.push_str("分布式缓存 kevy");
    }
    doc.into_bytes()
}

pub fn run() {
    println!("== kevy-text ==");

    let mut rng = Rng::new(0x7e57_7e57);
    let docs: Vec<(Vec<u8>, Vec<u8>)> =
        (0..10_000).map(|i| (format!("doc:{i:05}").into_bytes(), gen_doc(&mut rng, i))).collect();

    // ~1 KB mixed tokenize input, fixed seed.
    let mut sample_text = Vec::new();
    while sample_text.len() < 1024 {
        sample_text.extend_from_slice(&gen_doc(&mut rng, 1));
    }
    let s = bench(30, 200, || {
        black_box(tokenize(black_box(&sample_text)));
    });
    crate::row(&format!("tokenize {} B mixed CJK+ASCII", sample_text.len()), s, 1);

    let s = bench(7, 1, || {
        let mut seg = TextSegment::new();
        for (key, doc) in &docs {
            seg.apply(key, Some(doc));
        }
        black_box(seg.stats());
    });
    crate::row("segment build 10k docs (per doc)", s, docs.len());

    let mut seg = TextSegment::new();
    for (key, doc) in &docs {
        seg.apply(key, Some(doc));
    }
    let hits = seg.matches("分布式缓存 kevy".as_bytes(), 10).len();
    let s = bench(30, 20, || {
        black_box(seg.matches(black_box("分布式缓存 kevy".as_bytes()), 10));
    });
    crate::row(&format!("query limit=10 ({hits} hits)"), s, 1);
    println!();
}
