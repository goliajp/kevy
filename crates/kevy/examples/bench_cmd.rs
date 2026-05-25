//! Local command CPU micro-bench — the parse → dispatch → encode path one
//! connection pays per command (no socket / reactor / fold). This is the
//! kevy-rt-side per-command cost the single-shard ceiling (~3.77M GET/core,
//! ~265 ns/cmd) is built on; this bench isolates the CPU part so the stones can
//! be polished one by one. Ratios/absolutes indicative under host load.
//!
//! `cargo run -p kevy --example bench_cmd --release`

use kevy::{KeyspaceStore as Store, dispatch};
use kevy_bench::{bench, black_box, report};
use kevy_resp::parse_command;

const SAMPLES: usize = 80;
const INNER: usize = 50_000;

fn argv_of(bytes: &[u8]) -> kevy::Argv {
    parse_command(bytes).unwrap().unwrap().0
}

fn main() {
    let mut store = Store::new();
    let _ = dispatch(
        &mut store,
        &argv_of(b"*3\r\n$3\r\nSET\r\n$5\r\nkey42\r\n$16\r\nvalue-payload-16\r\n"),
    );
    let _ = dispatch(&mut store, &argv_of(b"*3\r\n$3\r\nSET\r\n$3\r\nctr\r\n$1\r\n0\r\n"));

    let get = b"*2\r\n$3\r\nGET\r\n$5\r\nkey42\r\n".to_vec();
    let set = b"*3\r\n$3\r\nSET\r\n$5\r\nkey42\r\n$16\r\nvalue-payload-16\r\n".to_vec();
    let incr = b"*2\r\n$4\r\nINCR\r\n$3\r\nctr\r\n".to_vec();
    let ga = argv_of(&get);
    let sa = argv_of(&set);
    let ia = argv_of(&incr);

    println!("kevy local command CPU — parse + dispatch(+encode) (indicative under load)\n");

    println!("== parse only (single-alloc Argv) ==");
    report("parse GET", bench(SAMPLES, INNER, || {
        black_box(argv_of(black_box(&get)));
    }));
    report("parse SET", bench(SAMPLES, INNER, || {
        black_box(argv_of(black_box(&set)));
    }));

    println!("\n== dispatch only (pre-parsed argv; allocates the reply Vec) ==");
    report("dispatch GET (hit)", bench(SAMPLES, INNER, || {
        black_box(dispatch(black_box(&mut store), black_box(&ga)));
    }));
    report("dispatch SET", bench(SAMPLES, INNER, || {
        black_box(dispatch(black_box(&mut store), black_box(&sa)));
    }));
    report("dispatch INCR", bench(SAMPLES, INNER, || {
        black_box(dispatch(black_box(&mut store), black_box(&ia)));
    }));

    println!("\n== combined parse + dispatch (full per-command CPU) ==");
    report(
        "GET parse+dispatch",
        bench(SAMPLES, INNER, || {
            let a = argv_of(black_box(&get));
            black_box(dispatch(black_box(&mut store), &a));
        }),
    );
    report(
        "SET parse+dispatch",
        bench(SAMPLES, INNER, || {
            let a = argv_of(black_box(&set));
            black_box(dispatch(black_box(&mut store), &a));
        }),
    );
}
