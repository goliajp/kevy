//! A key is gone the moment its deadline passes, whatever the reaper's
//! cadence: a read compares against the clock, not against the last tick.

use std::time::Duration;

use kevy_embedded::{Config, Store};

#[test]
fn an_expired_key_is_not_read_between_reaper_ticks() {
    // a reaper slower than the test, so nothing refreshes a cached clock
    let store =
        Store::open(Config::default().with_reaper_interval(Duration::from_secs(10))).unwrap();
    store.set_with_ttl(b"long", b"stays", Duration::from_secs(60)).unwrap();
    store.set_with_ttl(b"short", b"goes", Duration::from_millis(50)).unwrap();
    assert_eq!(store.get(b"short").unwrap().as_deref(), Some(&b"goes"[..]));
    std::thread::sleep(Duration::from_millis(120));
    assert_eq!(store.get(b"short").unwrap(), None, "read past its deadline");
    assert_eq!(store.exists(&[b"short"]).unwrap(), 0);
    assert_eq!(store.mget(&[b"short", b"long"]).unwrap(), [None, Some(b"stays".to_vec())]);
}
