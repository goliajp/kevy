//! Tests for `ops_reconcile.rs`.
//!
//! The detector has to be trustworthy in both directions, because a
//! reconciler that only reports one of them reads as "clean" during the
//! failure it was written to catch.

use crate::Config;
use crate::store::Store;

fn s() -> Store {
    Store::open(Config::default().with_ttl_reaper_manual()).unwrap()
}

/// The cookbook §21 shape: a row implies one uniqueness claim.
fn claim_for(key: &[u8]) -> Vec<Vec<u8>> {
    let id = &key[b"user:".len()..];
    vec![[b"email:".as_slice(), id].concat()]
}

fn seeded() -> Store {
    let s = s();
    for i in 0..40 {
        let id = format!("{i}");
        s.hset(format!("user:{id}").as_bytes(), &[(b"name", id.as_bytes())]).unwrap();
        s.set(format!("email:{id}").as_bytes(), b"1").unwrap();
    }
    s
}

fn report(s: &Store) -> crate::ReconcileReport {
    s.snapshot().reconcile(b"user:", &[b"email:"], |k, _v| claim_for(k))
}

#[test]
fn a_consistent_keyspace_reconciles_clean() {
    let r = report(&seeded());
    assert!(r.is_clean(), "missing {} orphaned {}", r.missing_count, r.orphaned_count);
    assert_eq!(r.rows, 40);
    assert_eq!(r.expected, 40);
    assert!(!r.truncated());
}

#[test]
fn a_lost_claim_is_reported_missing() {
    let s = seeded();
    s.del(&[b"email:7"]).unwrap();
    let r = report(&s);
    assert!(!r.is_clean());
    assert_eq!(r.missing_count, 1);
    assert_eq!(r.missing, vec![b"email:7".to_vec()]);
    assert_eq!(r.orphaned_count, 0);
}

#[test]
fn a_claim_left_behind_is_reported_orphaned() {
    // The half-applied update: the row is gone (or renamed) but its old
    // claim survives. This is the direction a missing-only checker
    // cannot see, and the one that silently blocks a later insert.
    let s = seeded();
    s.del(&[b"user:7"]).unwrap();
    let r = report(&s);
    assert!(!r.is_clean());
    assert_eq!(r.orphaned_count, 1);
    assert_eq!(r.orphaned, vec![b"email:7".to_vec()]);
    assert_eq!(r.missing_count, 0);
    assert_eq!(r.rows, 39);
}

#[test]
fn both_directions_are_reported_together() {
    let s = seeded();
    s.del(&[b"user:1"]).unwrap(); // orphans email:1
    s.del(&[b"email:2"]).unwrap(); // loses email:2
    let r = report(&s);
    assert_eq!(r.missing, vec![b"email:2".to_vec()]);
    assert_eq!(r.orphaned, vec![b"email:1".to_vec()]);
}

#[test]
fn counts_stay_exact_when_samples_truncate() {
    let s = s();
    // 1500 rows, every claim missing — more than MAX_SAMPLES.
    for i in 0..1500 {
        s.hset(format!("user:{i}").as_bytes(), &[(b"name", b"x")]).unwrap();
    }
    let r = report(&s);
    assert_eq!(r.missing_count, 1500, "count is exact");
    assert_eq!(r.missing.len(), 1000, "samples are capped");
    assert!(r.truncated(), "and the report says the samples were cut");
}

#[test]
fn a_row_implying_several_keys_is_diffed_per_key() {
    let s = s();
    s.hset(b"user:9", &[(b"dept", b"eng")]).unwrap();
    s.set(b"email:9", b"1").unwrap();
    // dept:eng:users is implied but never written.
    let r = s.snapshot().reconcile(b"user:", &[b"email:", b"dept:"], |k, _v| {
        let id = &k[b"user:".len()..];
        vec![[b"email:".as_slice(), id].concat(), b"dept:eng:users".to_vec()]
    });
    assert_eq!(r.expected, 2);
    assert_eq!(r.missing, vec![b"dept:eng:users".to_vec()]);
}

#[test]
fn reconciling_is_consistent_under_concurrent_writes() {
    // The reason this runs on a Snapshot: a live scan would see writes
    // land between the row pass and the derived pass and report them as
    // drift. A clean system must reconcile clean while it is being
    // written to, or the detector cries wolf in exactly the situation a
    // boot-time check is least able to investigate.
    //
    // Note what the writer has to do for that to hold: write the row
    // and its claim in ONE transaction. A frozen view cannot hide a
    // writer that leaves the pair half-applied — reconciliation and
    // atomic writes are the same guarantee seen from two ends.
    let s = std::sync::Arc::new(seeded());
    let writer = {
        let s = std::sync::Arc::clone(&s);
        std::thread::spawn(move || {
            for i in 100..400 {
                let id = format!("{i}");
                s.atomic_all_shards(|c| {
                    c.hset(format!("user:{id}").as_bytes(), &[(b"name", b"x")])?;
                    c.set(format!("email:{id}").as_bytes(), b"1");
                    Ok::<_, crate::KevyError>(())
                })
                .unwrap();
            }
        })
    };
    for _ in 0..50 {
        let r = report(&s);
        assert!(
            r.is_clean(),
            "snapshot must not see a half-written pair: missing {} orphaned {}",
            r.missing_count,
            r.orphaned_count
        );
    }
    writer.join().unwrap();
}
