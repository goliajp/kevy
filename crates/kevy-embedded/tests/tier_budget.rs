//! T5 unified-budget surface (capacity arc, RFC §1 D3 / §2 B7/B8/B12):
//! builder parity for the auto / percent budget forms (real dir
//! stores, real probe), the `# Tiering` INFO gauges (present when
//! tiered, absent when not), the reserved-floor feed through the tick,
//! and the IDX.CREATE floor refusal. The refusal lives here rather
//! than in the dispatch oracle: it only manifests with tiering ON, and
//! the oracle's server runs a plain untiered store.

#![cfg(all(feature = "tier", not(target_arch = "wasm32")))]

use kevy_embedded::{Config, Store};

fn dispatch(s: &Store, argv: &[&[u8]]) -> Vec<u8> {
    let owned: Vec<Vec<u8>> = argv.iter().map(|a| a.to_vec()).collect();
    let mut out = Vec::new();
    s.dispatch_argv(&owned, &mut out);
    out
}

fn tiered_bytes(dir: &std::path::Path, budget: u64) -> Config {
    Config::default()
        .with_ttl_reaper_manual()
        .with_persist(dir)
        .with_tier_budget(budget)
}

#[test]
fn builder_auto_resolves_against_the_real_probe() {
    let dir = kevy_tmpdir::TmpDir::new("tier-t5-auto");
    let s = Store::open(
        Config::default()
            .with_ttl_reaper_manual()
            .with_persist(dir.path())
            .with_tier_budget_auto(),
    )
    .expect("auto budget must resolve on the dev host");
    let t = s.tier_info().expect("tiered store reports the section");
    assert!(t.tier_budget_bytes > 0, "auto resolves to a positive budget");
    // 0.70 × the bound must sit strictly under the whole bound.
    assert!(t.tier_budget_bytes < t.tier_budget_bytes / 70 * 100 + 100);
    // The store still works end to end.
    s.set(b"k", &[b'x'; 4096]).unwrap();
    assert!(s.debug_force_demote(b"k"));
    assert_eq!(s.get(b"k").unwrap().unwrap().len(), 4096);
}

#[test]
fn builder_percent_scales_from_the_same_probe() {
    let d_full = kevy_tmpdir::TmpDir::new("tier-t5-pct-full");
    let d_half = kevy_tmpdir::TmpDir::new("tier-t5-pct-half");
    let full = Store::open(
        Config::default()
            .with_ttl_reaper_manual()
            .with_persist(d_full.path())
            .with_tier_budget_percent(100),
    )
    .expect("100% resolves");
    let half = Store::open(
        Config::default()
            .with_ttl_reaper_manual()
            .with_persist(d_half.path())
            .with_tier_budget_percent(50),
    )
    .expect("50% resolves");
    let b_full = full.tier_info().unwrap().tier_budget_bytes;
    let b_half = half.tier_info().unwrap().tier_budget_bytes;
    assert!(b_full > 0 && b_half > 0);
    // Within rounding, 50% is half of 100% (the bound can move a
    // little between the two probes on Linux MemAvailable — allow 5%).
    let expect = b_full / 2;
    let band = expect / 20 + 1;
    assert!(
        b_half.abs_diff(expect) <= band,
        "50% budget {b_half} vs half-of-100% {expect} (band {band})"
    );
}

#[test]
fn builder_percent_out_of_range_refused_by_name() {
    for p in [0u8, 101] {
        let dir = kevy_tmpdir::TmpDir::new("tier-t5-pct-bad");
        let res = Store::open(
            Config::default()
                .with_ttl_reaper_manual()
                .with_persist(dir.path())
                .with_tier_budget_percent(p),
        );
        let err = match res {
            Err(e) => e,
            Ok(_) => panic!("out-of-range percent {p} must refuse"),
        };
        assert!(err.to_string().contains("1..=100"), "{p}: {err}");
    }
}

#[test]
fn mem_only_rejection_unchanged_for_all_forms() {
    for cfg in [
        Config::default().with_tier_budget(1 << 20),
        Config::default().with_tier_budget_auto(),
        Config::default().with_tier_budget_percent(50),
    ] {
        let err = match Store::open(cfg.with_ttl_reaper_manual()) {
            Err(e) => e,
            Ok(_) => panic!("mem-only must refuse"),
        };
        assert!(err.to_string().contains("memory-only"), "{err}");
    }
}

#[test]
fn info_gauges_present_when_tiered_absent_when_not() {
    // Untiered: no section — the untiered snapshot is unchanged.
    let plain = Store::open(Config::default().with_ttl_reaper_manual()).unwrap();
    assert!(plain.info().tiering.is_none());
    assert!(plain.tier_info().is_none());

    let dir = kevy_tmpdir::TmpDir::new("tier-t5-info");
    let budget = 1_000_000u64;
    let s = Store::open(tiered_bytes(dir.path(), budget)).unwrap();
    s.set(b"cold", &[b'x'; 4096]).unwrap();
    assert!(s.debug_force_demote(b"cold"));
    let t = s.info().tiering.expect("tiered store carries the section");
    assert_eq!(t.tier_budget_bytes, budget);
    assert_eq!(t.cold_keys, 1);
    assert!(t.cold_bytes >= 4096, "original weight of the cold value: {}", t.cold_bytes);
    assert_eq!(t.stub_bytes, 96, "short key: ENTRY_OVERHEAD only");
    assert_eq!(t.demotions_total, 1);
    assert_eq!(t.promotions_total, 0);
    assert_eq!(t.vlog_files, 1);
    assert!(t.vlog_size_bytes > 4096);
    assert_eq!(t.vlog_live_bytes, t.vlog_size_bytes);
    assert_eq!(t.vlog_epoch, 0);
    assert_eq!(t.index_reserved_bytes, 0, "no indexes declared yet");
    assert_eq!(
        t.tier_effective_target,
        budget * 19 / 20 - t.stub_bytes,
        "the unified target arithmetic surfaces in the gauges"
    );
}

#[test]
fn reserved_floor_feeds_through_the_manual_tick() {
    let dir = kevy_tmpdir::TmpDir::new("tier-t5-reserved");
    let s = Store::open(tiered_bytes(dir.path(), 10_000_000)).unwrap();
    for i in 0..50u32 {
        s.hset(format!("row:{i}").as_bytes(), &[(b"score".as_slice(), format!("{i}").as_bytes())])
            .unwrap();
    }
    s.idx_create(b"by_score", b"row:", b"score", kevy_embedded::IndexValType::I64, kevy_embedded::IndexKind::Range)
        .unwrap();
    assert_eq!(s.info().tiering.unwrap().index_reserved_bytes, 0, "not fed before a tick");
    s.tick();
    let t = s.info().tiering.unwrap();
    assert!(t.index_reserved_bytes > 0, "the tick feeds the index floor");
    assert_eq!(
        t.tier_effective_target,
        (10_000_000u64 * 19 / 20)
            .saturating_sub(t.index_reserved_bytes)
            .saturating_sub(t.stub_bytes),
    );
}

#[test]
fn idx_create_refused_when_the_floor_exceeds_the_budget() {
    let dir = kevy_tmpdir::TmpDir::new("tier-t5-floor");
    // A budget small enough that one real index's segment exceeds its
    // watermark; big enough that plain writes stay serviceable.
    let s = Store::open(tiered_bytes(dir.path(), 4096)).unwrap();
    for i in 0..200u32 {
        s.hset(format!("row:{i}").as_bytes(), &[(b"score".as_slice(), format!("{i}").as_bytes())])
            .unwrap();
    }
    // First index: at declare time the floor is still 0 — accepted.
    s.idx_create(b"idx1", b"row:", b"score", kevy_embedded::IndexValType::I64, kevy_embedded::IndexKind::Range)
        .expect("first index declares against an empty floor");
    // Second: the existing floor now exceeds the tiny budget — refused
    // by name, and byte-identical on the wire to the server's error.
    let err = s
        .idx_create(b"idx2", b"row:", b"score", kevy_embedded::IndexValType::I64, kevy_embedded::IndexKind::Range)
        .expect_err("the floor must refuse the second index");
    assert!(
        err.to_string().contains("index memory floor exceeds the tiering budget"),
        "{err}"
    );
    let wire = dispatch(
        &s,
        &[b"IDX.CREATE", b"idx3", b"ON", b"PREFIX", b"row:", b"FIELD", b"score", b"TYPE", b"i64", b"KIND", b"range"],
    );
    assert_eq!(wire, b"-ERR index memory floor exceeds the tiering budget\r\n".to_vec());
}

/// v4.1-V5: `reserved_bytes` is generation-cached — an idle tick reads
/// a number instead of walking every segment's stats. The cache's one
/// failure mode is staleness (a frozen floor silently corrupts the
/// demote target), so every mutating chokepoint is walked here and the
/// fed floor must track each one.
#[test]
fn the_reserved_floor_cache_never_serves_stale_floors() {
    let dir = kevy_tmpdir::TmpDir::new("tier-t5-cache");
    let s = Store::open(tiered_bytes(dir.path(), 10_000_000)).unwrap();
    let reserved = |s: &Store| s.info().tiering.unwrap().index_reserved_bytes;
    for i in 0..50u32 {
        s.hset(format!("row:{i}").as_bytes(), &[(b"score".as_slice(), format!("{i}").as_bytes())])
            .unwrap();
    }
    // Declare (rebuild chokepoint) — the floor appears.
    s.idx_create(b"by_score", b"row:", b"score", kevy_embedded::IndexValType::I64, kevy_embedded::IndexKind::Range)
        .unwrap();
    s.tick();
    let after_create = reserved(&s);
    assert!(after_create > 0, "declare grows the floor");
    // Pure idle — same number, now served from the cache.
    s.tick();
    assert_eq!(reserved(&s), after_create, "idle ticks change nothing");
    // Write applies (the on_commit chokepoint) — the floor grows.
    for i in 50..300u32 {
        s.hset(format!("row:{i}").as_bytes(), &[(b"score".as_slice(), format!("{i}").as_bytes())])
            .unwrap();
    }
    s.tick();
    let after_writes = reserved(&s);
    assert!(after_writes > after_create, "row writes grow the indexed floor");
    // FLUSHALL (the reset chokepoint) — the floor collapses.
    let out = dispatch(&s, &[b"FLUSHALL"]);
    assert_eq!(out, b"+OK\r\n".to_vec());
    s.tick();
    let after_flush = reserved(&s);
    assert!(after_flush < after_writes, "FLUSHALL resets the segments and the cache sees it");
    // Drop (catalog chokepoint) — back to zero.
    assert!(s.idx_drop(b"by_score"));
    s.tick();
    assert_eq!(reserved(&s), 0, "no index, no floor");
}
