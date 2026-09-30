//! Facade calls whose less travelled paths the other suites do not take:
//! the lending read under either lock, RANDOMKEY over a shard that holds
//! only expired keys, and what the public error and iterator types print.

use std::time::Duration;

use crate::{Config, EvictionPolicy, Store};

#[test]
fn get_with_lends_the_value_under_either_lock() {
    let shared = Store::open(Config::default().with_ttl_reaper_manual()).unwrap();
    let exclusive = Store::open(
        Config::default()
            .with_ttl_reaper_manual()
            .with_max_memory(64 << 20)
            .with_eviction(EvictionPolicy::AllKeysLru),
    )
    .unwrap();
    for s in [&shared, &exclusive] {
        s.set(b"k", b"value").unwrap();
        assert_eq!(s.get_with(b"k", |v| v.map(<[u8]>::len)).unwrap(), Some(5));
        assert!(s.get_with(b"missing", |v| v.is_none()).unwrap());
    }
}

#[test]
fn randomkey_skips_a_shard_that_holds_only_expired_keys() {
    let s = Store::open(Config::default().with_ttl_reaper_manual().with_shards(4)).unwrap();
    for i in 0..64 {
        let key = format!("gone:{i}");
        s.set_with_ttl(key.as_bytes(), b"x", Duration::from_millis(1)).unwrap();
    }
    std::thread::sleep(Duration::from_millis(20));
    s.set(b"live", b"1").unwrap();
    for _ in 0..64 {
        assert_eq!(s.randomkey(), Some(b"live".to_vec()));
    }
}

#[test]
fn keys_iter_prints_where_it_stands() {
    let s = Store::open(Config::default().with_ttl_reaper_manual()).unwrap();
    let it = s.keys_iter(Some(b"user:*"));
    assert_eq!(
        format!("{it:?}"),
        r#"KeysIter { pattern: Some([117, 115, 101, 114, 58, 42]), cursor: 0, done: false, .. }"#
    );
}

#[cfg(all(feature = "replicate", not(target_arch = "wasm32")))]
#[test]
fn feed_errors_say_what_went_wrong() {
    use crate::{FeedError, FeedPosition};
    let tail = FeedPosition::new(3, 17);
    let cases = [
        (
            FeedError::Resync { tail },
            "feed cursor unservable, resync from generation 3 offset 17",
            "ERR feed: Resync { generation: 3, tail: 17 }",
        ),
        (FeedError::Future, "feed cursor ahead of stream", "ERR feed: Future"),
        (FeedError::Disabled, "the store was opened without a change feed", "ERR feed: Disabled"),
    ];
    for (e, shown, wire) in cases {
        assert_eq!(e.to_string(), shown);
        assert_eq!(e.wire_text(), wire);
    }
}
