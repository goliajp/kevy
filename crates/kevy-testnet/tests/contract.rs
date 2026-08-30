#[test]
#[should_panic(expected = "never accepted on 127.0.0.1")]
fn a_server_that_never_binds_is_loud() {
    // Nothing is started here on purpose: the old loops polled, gave up,
    // and returned as if the server were ready.
    kevy_testnet::assert_listening(kevy_testnet::free_port(), "a server nobody started");
}

#[test]
fn a_port_in_use_is_never_handed_out_again() {
    // The contract is not "the numbers never repeat" — a block is finite
    // and a port returns to the pool when whatever held it stops. It is
    // that a port somebody is *holding* is never given to somebody else,
    // which is what free_port's bind check establishes. Hold them to test
    // it; collecting the numbers without holding them tests nothing and
    // fails on recycling, which is correct behaviour.
    let held: Vec<_> = (0..24)
        .map(|_| {
            let p = kevy_testnet::free_port();
            (p, std::net::TcpListener::bind(("127.0.0.1", p)).expect("free when handed out"))
        })
        .collect();
    let mut seen: Vec<u16> = held.iter().map(|(p, _)| *p).collect();
    let n = seen.len();
    seen.sort_unstable();
    seen.dedup();
    assert_eq!(n, seen.len(), "a port that was still held came back");
}

#[test]
fn a_block_is_contiguous_and_free() {
    let base = kevy_testnet::free_port_block(4);
    for i in 0..4u16 {
        std::net::TcpListener::bind(("127.0.0.1", base + i))
            .expect("every port in the run is free");
    }
}

#[test]
fn a_block_covers_the_base_and_the_run_after_it() {
    // The callers were written against "the port returned is free, and so
    // are the `width` after it" — `free_port_block(n)` then using
    // `base + 1 ..= base + n`. A version that reserved only `n` starting
    // at the base handed out a run whose tail nobody had checked, and the
    // cluster tests found it as a connection reset.
    let width = 4;
    let base = kevy_testnet::free_port_block(width);
    for i in 0..=width as u16 {
        std::net::TcpListener::bind(("127.0.0.1", base + i))
            .unwrap_or_else(|e| panic!("base+{i} must be free too: {e}"));
    }
}

#[test]
fn a_reserved_run_is_not_handed_out_while_it_is_held() {
    // Same distinction as above: hold the run the way a caller does — by
    // starting something on it — and free_port must route around it.
    let width = 3u16;
    let base = kevy_testnet::free_port_block(width as usize);
    let _held: Vec<_> = (0..=width)
        .map(|i| std::net::TcpListener::bind(("127.0.0.1", base + i)).expect("run is free"))
        .collect();
    let run: Vec<u16> = (0..=width).map(|i| base + i).collect();
    for _ in 0..24 {
        let p = kevy_testnet::free_port();
        let _hold = std::net::TcpListener::bind(("127.0.0.1", p));
        assert!(!run.contains(&p), "free_port handed out {p}, inside the held run {run:?}");
    }
}
