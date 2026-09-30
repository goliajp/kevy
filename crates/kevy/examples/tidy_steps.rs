//! tidy_steps — every repack step of one index segment, timed, with no
//! server around it.
//!
//!   cargo run --release -p kevy --features harness-repack-trace --example tidy_steps -- \
//!       [rows=1000000] [writes=0] [evict_mb=0] [long_us=100] [max_steps=200000]
//!
//! Fills a segment in random value order, then repacks it four leaves a
//! step, the server's step size. Between steps it can apply `writes`
//! random moves (a shard's writes between ticks) and walk `evict_mb` MiB
//! of unrelated memory (the cache a shard's other work leaves cold).
//!
//! Prints the step-time histogram, the share of slow steps among steps
//! that did and did not free a leaf, take a page fault, or lose the CPU,
//! and each slow step with its thread CPU time next to its wall time: a
//! slow step whose CPU time is its wall time spent it computing (or in the
//! kernel on its behalf); one whose CPU time is short was off the CPU.

use std::collections::HashMap;
use std::time::Instant;

use kevy_index::{IndexValue, Segment, TidyProbe};
use kevy_sys::ThreadUsage;

struct Slow {
    n: u64,
    ns: u64,
    u: [ThreadUsage; 2],
    p: [TidyProbe; 2],
}

fn main() {
    let kv: HashMap<String, u64> = std::env::args()
        .skip(1)
        .filter_map(|a| {
            let (k, v) = a.split_once('=')?;
            Some((k.to_string(), v.parse().ok()?))
        })
        .collect();
    let arg = |k: &str, d: u64| kv.get(k).copied().unwrap_or(d);
    let (rows, writes, long_ns) =
        (arg("rows", 1_000_000), arg("writes", 0), arg("long_us", 100) * 1000);
    let mut x = 0x9E37_79B9_7F4A_7C15u64;
    let (mut s, mut vals) = fill(rows, &mut x);
    let mut junk = vec![0u8; (arg("evict_mb", 0) << 20) as usize];
    // per class (bit 0 freed a leaf, bit 1 faulted, bit 2 lost the CPU): steps, slow steps
    let (mut hist, mut class, mut slow) = ([0u64; 64], [[0u64; 2]; 8], Vec::new());
    let (t, mut steps) = (Instant::now(), 0u64);
    loop {
        for _ in 0..writes {
            let i = (xorshift(&mut x) % rows) as usize;
            let v = (xorshift(&mut x) % (rows * 16)) as i64;
            let (old, new) = (IndexValue::I64(vals[i]), IndexValue::I64(v));
            s.apply(format!("r:{i}").as_bytes(), Some(&old), Some(new));
            vals[i] = v;
        }
        junk.iter_mut().step_by(64).for_each(|b| *b = b.wrapping_add(1));
        let (u0, p0) = (usage(), s.tidy_probe());
        let t0 = Instant::now();
        let more = s.tidy(4);
        let ns = t0.elapsed().as_nanos() as u64;
        let (u1, p1) = (usage(), s.tidy_probe());
        steps += 1;
        hist[63 - ns.max(1).leading_zeros() as usize] += 1;
        let c = classify(&u0, &u1, &p0, &p1);
        class[c][0] += 1;
        if ns >= long_ns {
            class[c][1] += 1;
            slow.push(Slow { n: steps, ns, u: [u0, u1], p: [p0, p1] });
        }
        if !more || steps >= arg("max_steps", 200_000) {
            break;
        }
    }
    let p = s.tidy_probe();
    println!(
        "repack steps={steps} secs={:.2} leaves={} entries={}",
        t.elapsed().as_secs_f64(),
        p.leaves,
        p.entries
    );
    for (e, c) in hist.iter().enumerate().filter(|(_, c)| **c > 0) {
        println!("  [{:>9} ns, {:>9} ns) {c}", 1u64 << e, 2u64 << e);
    }
    println!(
        "slow = at or over {} us; by class (freed leaf / faulted / lost cpu):",
        long_ns / 1000
    );
    for (c, [n, k]) in class.iter().enumerate().filter(|(_, v)| v[0] > 0) {
        let yes = |bit: usize| if c & bit != 0 { "y" } else { "-" };
        println!(
            "  {} {} {}  steps={n:<8} slow={k:<6} ({:.3}%)",
            yes(1),
            yes(2),
            yes(4),
            100.0 * *k as f64 / *n as f64
        );
    }
    for r in slow.iter().take(40) {
        println!("  {}", describe(r));
    }
}

fn fill(rows: u64, x: &mut u64) -> (Segment, Vec<i64>) {
    let (mut s, mut vals) = (Segment::new(), Vec::with_capacity(rows as usize));
    let t = Instant::now();
    for i in 0..rows {
        let v = (xorshift(x) % (rows * 16)) as i64;
        s.apply(format!("r:{i}").as_bytes(), None, Some(IndexValue::I64(v)));
        vals.push(v);
    }
    let p = s.tidy_probe();
    println!(
        "fill rows={rows} secs={:.2} leaves={} height={}",
        t.elapsed().as_secs_f64(),
        p.leaves,
        p.height
    );
    (s, vals)
}

fn usage() -> ThreadUsage {
    kevy_sys::thread_usage().unwrap_or_default()
}

fn classify(u0: &ThreadUsage, u1: &ThreadUsage, p0: &TidyProbe, p1: &TidyProbe) -> usize {
    let freed = p1.leaves < p0.leaves;
    let faulted = u1.minor_faults + u1.major_faults > u0.minor_faults + u0.major_faults;
    let switched = u1.voluntary_switches + u1.involuntary_switches
        > u0.voluntary_switches + u0.involuntary_switches;
    usize::from(freed) | usize::from(faulted) << 1 | usize::from(switched) << 2
}

fn describe(r: &Slow) -> String {
    let ([u0, u1], [a, b]) = (&r.u, &r.p);
    format!(
        "step {}: wall {} us, cpu {} us, minflt {}, nvcsw {}, nivcsw {}, leaves {}->{}, inners {}->{}, \
         moved {}, free_list_cap {}->{}",
        r.n,
        r.ns / 1000,
        u1.cpu_ns.saturating_sub(u0.cpu_ns) / 1000,
        u1.minor_faults - u0.minor_faults,
        u1.voluntary_switches - u0.voluntary_switches,
        u1.involuntary_switches - u0.involuntary_switches,
        a.leaves,
        b.leaves,
        a.inners,
        b.inners,
        b.lap_moved.saturating_sub(a.lap_moved),
        a.free_list_cap,
        b.free_list_cap
    )
}

fn xorshift(x: &mut u64) -> u64 {
    *x ^= *x << 13;
    *x ^= *x >> 7;
    *x ^= *x << 17;
    *x
}
