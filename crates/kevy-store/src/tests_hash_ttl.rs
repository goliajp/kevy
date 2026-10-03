//! Field TTL codes, deadlines and the lazy purge.

use super::*;

fn h(s: &mut Store) {
    s.hset(b"h", &[(b"a".as_slice(), b"1".as_slice()), (b"b".as_slice(), b"2".as_slice())])
        .unwrap();
}

#[test]
fn hexpire_httl_hpersist_codes() {
    let mut s = Store::new();
    h(&mut s);
    let far = now_unix_ms() + 100_000;
    // set on a + missing field
    let codes = s.hexpire_at(b"h", &[b"a", b"nope"], far, HExpireCond::Always).unwrap();
    assert_eq!(codes, vec![1, -2]);
    let ttls = s.hpttl(b"h", &[b"a", b"b", b"nope"]).unwrap();
    assert!(ttls[0] > 90_000 && ttls[0] <= 100_000);
    assert_eq!(&ttls[1..], &[-1, -2]);
    // NX refuses existing, XX refuses missing
    assert_eq!(s.hexpire_at(b"h", &[b"a"], far + 1, HExpireCond::Nx).unwrap(), vec![0]);
    assert_eq!(s.hexpire_at(b"h", &[b"b"], far, HExpireCond::Xx).unwrap(), vec![0]);
    // GT/LT
    assert_eq!(s.hexpire_at(b"h", &[b"a"], far + 500, HExpireCond::Gt).unwrap(), vec![1]);
    assert_eq!(s.hexpire_at(b"h", &[b"a"], far, HExpireCond::Gt).unwrap(), vec![0]);
    // persist
    assert_eq!(s.hpersist(b"h", &[b"a", b"b", b"nope"]).unwrap(), vec![1, -1, -2]);
    assert_eq!(s.hpttl(b"h", &[b"a"]).unwrap(), vec![-1]);
}

#[test]
fn past_deadline_deletes_and_lazy_purge_enforces() {
    let mut s = Store::new();
    h(&mut s);
    // past deadline → immediate delete, code 2
    assert_eq!(s.hexpire_at(b"h", &[b"a"], 1, HExpireCond::Always).unwrap(), vec![2]);
    assert!(!s.hexists(b"h", b"a").unwrap());
    // near-future deadline → lazily gone after it passes
    let soon = now_unix_ms() + 30;
    s.hexpire_at(b"h", &[b"b"], soon, HExpireCond::Always).unwrap();
    std::thread::sleep(core::time::Duration::from_millis(50));
    assert!(!s.hexists(b"h", b"b").unwrap(), "lazy purge on access");
    // hash is now empty → hlen 0, sidecar pruned
    assert_eq!(s.hlen(b"h").unwrap(), 0);
    assert!(s.hfttl.is_empty());
}

#[test]
fn overwrite_clears_ttl_and_reaper_reports() {
    let mut s = Store::new();
    h(&mut s);
    let soon = now_unix_ms() + 20;
    s.hexpire_at(b"h", &[b"a", b"b"], soon, HExpireCond::Always).unwrap();
    // overwrite a → its TTL is discarded (Redis 7.4)
    s.hset(b"h", &[(b"a".as_slice(), b"new".as_slice())]).unwrap();
    assert_eq!(s.hpttl(b"h", &[b"a"]).unwrap(), vec![-1]);
    std::thread::sleep(core::time::Duration::from_millis(40));
    // reaper sweeps b, reports the removal for effect logging
    let swept = s.tick_hash_ttl(100);
    assert_eq!(swept, vec![(b"h".to_vec(), vec![b"b".to_vec()])]);
    assert!(s.hexists(b"h", b"a").unwrap(), "overwritten field survived");
    // whole-key delete drops the sidecar
    let far = now_unix_ms() + 100_000;
    s.hexpire_at(b"h", &[b"a"], far, HExpireCond::Always).unwrap();
    s.del(&[b"h".as_slice()]);
    assert!(s.hfttl.is_empty());
}
