//! FLUSHALL breaks the change feed's stream continuity, and the way it
//! says so is the generation number.
//!
//! `feed_bump_on_flush` implements that contract, and until this file
//! existed nothing asked it to. It was being covered by accident: the
//! wire/facade differential's arity probe sent every shared verb its
//! bare form, FLUSHALL's bare form is a COMPLETE call, and the store
//! that probe opens has the feed on — so the flush happened, and the
//! function ran, as a side effect of a test about arity. Teaching that
//! probe to skip verbs whose bare call is complete took the coverage
//! away with it, and `deadgate` named the function the same day.
//!
//! Coverage acquired that way is coverage nobody can reason about. This
//! asks for the behaviour directly.

use kevy_embedded::{Config, Store};

/// `(generation, next_offset)` as FEED.TAIL reports them.
fn tail(store: &Store) -> (u64, u64) {
    let mut out = Vec::new();
    store.dispatch_argv(&[b"FEED.TAIL".to_vec()], &mut out);
    let text = String::from_utf8_lossy(&out).to_string();
    let parts: Vec<&str> = text.split("\r\n").collect();
    assert_eq!(parts.first(), Some(&"*2"), "FEED.TAIL did not answer a pair: {text:?}");
    let int = |s: &str| -> u64 {
        s.strip_prefix(':')
            .and_then(|d| d.parse().ok())
            .unwrap_or_else(|| panic!("FEED.TAIL element is not an integer: {text:?}"))
    };
    (int(parts[1]), int(parts[2]))
}

#[test]
fn flushall_bumps_the_feed_generation_and_writes_it_down() {
    let dir = kevy_tmpdir::TmpDir::new("feed-gen-flush");
    let cfg = Config::default()
        .with_persist(dir.path().to_str().unwrap())
        .with_feed(1 << 20);
    let store = Store::open(cfg).expect("open");

    for i in 0..8 {
        store.set(format!("k{i}").as_bytes(), b"v").unwrap();
    }
    let (gen_before, off_before) = tail(&store);
    assert!(off_before > 0, "eight writes produced no feed offset");

    let mut out = Vec::new();
    store.dispatch_argv(&[b"FLUSHALL".to_vec()], &mut out);
    assert_eq!(String::from_utf8_lossy(&out), "+OK\r\n");

    // A generation is a history IDENTITY, not a counter — `fresh_generation`
    // draws one rather than adding one, precisely so two nodes cannot both
    // call their history "2". So the claim is that it CHANGED, not that it
    // grew, and that the offsets restarted with it.
    let (gen_after, off_after) = tail(&store);
    assert_ne!(
        gen_after, gen_before,
        "FLUSHALL left the generation at {gen_before}: a reader holding an \
         offset from before the flush would keep reading as if the stream \
         had never been cut"
    );
    assert_eq!(off_after, 0, "the new generation did not start its offsets over");

    // And it is on disk, not only in the handle: the high-water is
    // persisted so a restart cannot hand the pre-flush generation back
    // out. `feed_meta` has no public reader, so this reads the file it
    // documents — `feed-<shard>.gen` — and would fail loudly if that
    // name ever changed, which is the correct thing for a test that is
    // asserting something was written down.
    let gen_file = dir.path().join("feed-0.gen");
    let persisted: u64 = std::fs::read_to_string(&gen_file)
        .unwrap_or_else(|e| panic!("no {} after FLUSHALL: {e}", gen_file.display()))
        .trim()
        .parse()
        .expect("the generation file holds a decimal");
    assert_eq!(
        persisted, gen_after,
        "the drawn generation was not the one written down"
    );
}
