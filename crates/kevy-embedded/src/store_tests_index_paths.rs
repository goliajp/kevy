//! Every way a row changes keeps a scalar index equal to the rows: the
//! facade, transactions and their rollbacks, expiry, field expiry,
//! renames, and frames applied from a log or a primary. The index finds a
//! row's old entry from the store's record of the row before its write,
//! so these are the paths that record has to cover.

use std::time::Duration;

use kevy_store::HExpireCond;

use crate::{Config, IndexKind, IndexValType, IndexValue, Store};

/// What the index should hold: every `ix:` row with a numeric `n`.
fn truth(s: &Store) -> Vec<(Vec<u8>, i64)> {
    let mut out = Vec::new();
    for k in s.keys(Some(b"ix:*"), None) {
        if let Ok(Some(v)) = s.hget(&k, b"n")
            && let Some(n) = std::str::from_utf8(&v).ok().and_then(|t| t.trim().parse::<i64>().ok())
        {
            out.push((k, n));
        }
    }
    out.sort_by(|a, b| (a.1, &a.0).cmp(&(b.1, &b.0)));
    out
}

fn held(s: &Store) -> Vec<(Vec<u8>, i64)> {
    let (hits, _) = s
        .idx_query(b"ix_n", &IndexValue::I64(i64::MIN), &IndexValue::I64(i64::MAX), None, 10_000)
        .unwrap();
    hits.into_iter()
        .map(|(k, v)| match v {
            IndexValue::I64(n) => (k, n),
            other => panic!("an i64 index held {other:?}"),
        })
        .collect()
}

fn check(s: &Store, after: &str) {
    assert_eq!(held(s), truth(s), "the index and the rows disagree after {after}");
}

fn seeded() -> Store {
    let s = Store::open(Config::default().with_ttl_reaper_manual()).unwrap();
    for i in 0..20 {
        s.hset(format!("ix:{i}").as_bytes(), &[(b"n", format!("{i}").as_bytes()), (b"f", b"x")])
            .unwrap();
    }
    s.idx_create(b"ix_n", b"ix:", b"n", IndexValType::I64, IndexKind::Range).unwrap();
    check(&s, "the build");
    s
}

#[test]
fn facade_writes_and_renames() {
    let s = seeded();
    s.hset(b"ix:1", &[(b"n", b"100")]).unwrap();
    s.hset(b"ix:2", &[(b"f", b"only an unindexed field")]).unwrap();
    s.hdel(b"ix:3", &[b"n"]).unwrap();
    s.del(&[b"ix:4", b"ix:5"]).unwrap();
    s.rename(b"ix:6", b"ix:600").unwrap();
    s.rename(b"ix:7", b"other:7").unwrap();
    s.set(b"ix:8", b"a string now").unwrap();
    check(&s, "facade writes");
}

#[test]
fn a_rolled_back_transaction_leaves_the_index_as_it_was() {
    let s = seeded();
    let r: crate::KevyResult<()> = s.atomic(|t| {
        t.hset(b"ix:1", &[(b"n", b"500")])?;
        t.del(&[b"ix:2"]);
        t.hset(b"ix:new", &[(b"n", b"7")])?;
        Err(crate::KevyError::InvalidInput("rejected".into()))
    });
    assert!(r.is_err());
    check(&s, "a rollback");
    s.atomic(|t| {
        t.hset(b"ix:1", &[(b"n", b"500")])?;
        t.del(&[b"ix:2"]);
        t.hset(b"ix:new", &[(b"n", b"7")])?;
        Ok(())
    })
    .unwrap();
    check(&s, "a commit");
}

#[test]
fn expiry_and_field_expiry() {
    let s = seeded();
    s.expire(b"ix:1", Duration::from_millis(5)).unwrap();
    s.hexpire(b"ix:2", &[b"n"], Duration::from_millis(5), HExpireCond::Always).unwrap();
    std::thread::sleep(Duration::from_millis(20));
    // read paths reap them; no write names either row
    assert_eq!(s.hget(b"ix:1", b"n").unwrap(), None);
    assert_eq!(s.hget(b"ix:2", b"n").unwrap(), None);
    check(&s, "lazy expiry on reads");
    s.expire(b"ix:3", Duration::from_millis(5)).unwrap();
    std::thread::sleep(Duration::from_millis(20));
    for _ in 0..64 {
        s.tick();
    }
    check(&s, "the reaper");
}

#[cfg(feature = "persist")]
#[test]
fn frames_applied_from_a_log_or_a_primary() {
    let s = seeded();
    let frame = |parts: &[&[u8]]| {
        let mut a = kevy_persist::Argv::default();
        for p in parts {
            a.push(p);
        }
        a
    };
    s.apply_frame(&frame(&[b"HSET", b"ix:1", b"n", b"77"]));
    s.apply_frame(&frame(&[b"DEL", b"ix:2", b"ix:3"]));
    s.apply_frame(&frame(&[b"HSET", b"ix:fresh", b"n", b"-4"]));
    check(&s, "applied frames");
}
