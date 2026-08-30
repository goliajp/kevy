//! Tests for the transaction-scoped index reads in
//! `ops_atomic_all_index.rs`.
//!
//! The point of these reads is to let a uniqueness check consult a
//! declared index instead of a parallel set of claim keys, so the two
//! properties worth pinning are that the check sees committed rows on
//! every shard, and that it does not see the transaction's own writes.
//! The second is a documented limit, and a test is the only thing that
//! keeps a documented limit from quietly becoming a different one.

use crate::store::Store;
use crate::{Config, IndexKind, IndexValType, IndexValue};

fn s() -> Store {
    Store::open(Config::default().with_ttl_reaper_manual()).unwrap()
}

fn with_email_index() -> Store {
    let s = s();
    // Enough rows that they land on more than one shard — the whole
    // reason these live on the all-shards context.
    for i in 0..64 {
        s.hset(
            format!("user:{i}").as_bytes(),
            &[(b"email", format!("u{i}@example.com").as_bytes())],
        )
        .unwrap();
    }
    s.idx_create(b"email_idx", b"user:", b"email", IndexValType::Str, IndexKind::Range).unwrap();
    s
}

fn eq(v: &str) -> IndexValue {
    IndexValue::Str(v.as_bytes().to_vec())
}

#[test]
fn atomic_all_shards_sees_committed_rows_on_every_shard() {
    let s = with_email_index();
    // Every one of the 64 rows must be findable from inside the
    // transaction, whichever shard it hashed to. A read that only
    // consulted the caller's own shard would miss most of them, which
    // is exactly the failure a uniqueness check must not have.
    s.atomic_all_shards(|c| {
        for i in 0..64 {
            let taken = c
                .idx_count(
                    b"email_idx",
                    &eq(&format!("u{i}@example.com")),
                    &eq(&format!("u{i}@example.com")),
                )
                .unwrap();
            assert_eq!(taken, 1, "u{i}@example.com should be visible in-transaction");
        }
        Ok::<_, crate::KevyError>(())
    })
    .unwrap();
}

#[test]
fn atomic_all_shards_uniqueness_check_rejects_a_taken_email() {
    let s = with_email_index();
    let e = eq("u7@example.com");
    let outcome = s.atomic_all_shards(|c| {
        if c.idx_count(b"email_idx", &e, &e)? > 0 {
            return Err(crate::KevyError::NotFound("email taken".into()));
        }
        c.hset(b"user:new", &[(b"email", b"u7@example.com")])?;
        Ok(())
    });
    assert!(outcome.is_err(), "the transaction should have been rejected");
    // And the rejection rolled back, so the duplicate never landed.
    assert!(s.hgetall(b"user:new").unwrap().is_empty());
}

#[test]
fn atomic_all_shards_query_returns_the_key() {
    let s = with_email_index();
    s.atomic_all_shards(|c| {
        let e = eq("u42@example.com");
        let (hits, next) = c.idx_query(b"email_idx", &e, &e, None, 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].0, b"user:42".to_vec());
        assert!(next.is_none());
        Ok::<_, crate::KevyError>(())
    })
    .unwrap();
}

#[test]
fn atomic_all_shards_index_read_does_not_see_its_own_writes() {
    let s = with_email_index();
    // Documented limit: index maintenance runs at commit, so a row
    // written earlier in the same closure is not yet indexed. A caller
    // inserting two rows has to compare them to each other directly.
    s.atomic_all_shards(|c| {
        let e = eq("brand-new@example.com");
        assert_eq!(c.idx_count(b"email_idx", &e, &e).unwrap(), 0);
        c.hset(b"user:brand-new", &[(b"email", b"brand-new@example.com")])?;
        assert_eq!(
            c.idx_count(b"email_idx", &e, &e).unwrap(),
            0,
            "own write must not be visible to the index read yet"
        );
        Ok::<_, crate::KevyError>(())
    })
    .unwrap();
    // ...but it is indexed once the transaction commits.
    let e = eq("brand-new@example.com");
    assert_eq!(s.idx_count(b"email_idx", &e, &e).unwrap(), 1);
}

#[test]
fn atomic_all_shards_unknown_index_is_an_error() {
    let s = with_email_index();
    s.atomic_all_shards(|c| {
        let e = eq("whatever");
        assert!(c.idx_count(b"no_such_idx", &e, &e).is_err());
        Ok::<_, crate::KevyError>(())
    })
    .unwrap();
}
