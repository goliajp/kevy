use super::*;

#[test]
fn embedded_hash_methods() {
    let mut c = Connection::open("mem://").unwrap();
    let pairs: &[(&[u8], &[u8])] = &[
        (b"name".as_ref(), b"alice".as_ref()),
        (b"age".as_ref(), b"30".as_ref()),
    ];
    assert_eq!(c.hset(b"u:1", pairs).unwrap(), 2);
    assert_eq!(c.hget(b"u:1", b"name").unwrap(), Some(b"alice".to_vec()));
    assert_eq!(c.hget(b"u:1", b"missing").unwrap(), None);
    assert_eq!(c.hlen(b"u:1").unwrap(), 2);

    let mut all = c.hgetall(b"u:1").unwrap();
    all.sort();
    assert!(all.contains(&b"alice".to_vec()));
    assert!(all.contains(&b"name".to_vec()));

    let mut keys = c.hkeys(b"u:1").unwrap();
    keys.sort();
    assert_eq!(keys, vec![b"age".to_vec(), b"name".to_vec()]);

    let mut vals = c.hvals(b"u:1").unwrap();
    vals.sort();
    assert_eq!(vals, vec![b"30".to_vec(), b"alice".to_vec()]);

    assert_eq!(c.hdel(b"u:1", &[&b"age"[..], &b"missing"[..]]).unwrap(), 1);
    assert_eq!(c.hlen(b"u:1").unwrap(), 1);
}

#[test]
fn embedded_list_methods() {
    let mut c = Connection::open("mem://").unwrap();
    assert_eq!(c.rpush(b"q", &[&b"a"[..], &b"b"[..], &b"c"[..]]).unwrap(), 3);
    assert_eq!(c.lpush(b"q", &[&b"z"[..]]).unwrap(), 4);
    assert_eq!(c.llen(b"q").unwrap(), 4);

    assert_eq!(
        c.lrange(b"q", 0, -1).unwrap(),
        vec![b"z".to_vec(), b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]
    );

    assert_eq!(c.lpop(b"q", 2).unwrap(), vec![b"z".to_vec(), b"a".to_vec()]);
    assert_eq!(c.rpop(b"q", 1).unwrap(), vec![b"c".to_vec()]);
    assert_eq!(c.llen(b"q").unwrap(), 1);
}

#[test]
fn embedded_set_methods() {
    let mut c = Connection::open("mem://").unwrap();
    assert_eq!(
        c.sadd(b"s", &[&b"a"[..], &b"b"[..], &b"a"[..]]).unwrap(),
        2
    );
    assert_eq!(c.scard(b"s").unwrap(), 2);
    assert!(c.sismember(b"s", b"a").unwrap());
    assert!(!c.sismember(b"s", b"missing").unwrap());

    let mut m = c.smembers(b"s").unwrap();
    m.sort();
    assert_eq!(m, vec![b"a".to_vec(), b"b".to_vec()]);

    assert_eq!(c.srem(b"s", &[&b"a"[..]]).unwrap(), 1);
    assert_eq!(c.scard(b"s").unwrap(), 1);
}

#[test]
fn embedded_zset_methods() {
    let mut c = Connection::open("mem://").unwrap();
    let pairs: &[(f64, &[u8])] = &[
        (100.0, b"alice".as_ref()),
        (200.0, b"bob".as_ref()),
        (50.0, b"carol".as_ref()),
    ];
    assert_eq!(c.zadd(b"lb", pairs).unwrap(), 3);
    assert_eq!(c.zscore(b"lb", b"bob").unwrap(), Some(200.0));
    assert_eq!(c.zscore(b"lb", b"none").unwrap(), None);
    assert_eq!(c.zcard(b"lb").unwrap(), 3);

    let r = c.zrange(b"lb", 0, -1).unwrap();
    assert_eq!(
        r,
        vec![b"carol".to_vec(), b"alice".to_vec(), b"bob".to_vec()]
    );

    assert_eq!(c.zrem(b"lb", &[&b"carol"[..]]).unwrap(), 1);
    assert_eq!(c.zcard(b"lb").unwrap(), 2);
}

#[test]
fn embedded_set_combine_ops() {
    let mut c = Connection::open("mem://").unwrap();
    c.sadd(b"a", &[&b"x"[..], &b"y"[..], &b"z"[..]]).unwrap();
    c.sadd(b"b", &[&b"y"[..], &b"z"[..], &b"w"[..]]).unwrap();

    let mut inter = c.sinter(&[&b"a"[..], &b"b"[..]]).unwrap();
    inter.sort();
    assert_eq!(inter, vec![b"y".to_vec(), b"z".to_vec()]);

    let mut union = c.sunion(&[&b"a"[..], &b"b"[..]]).unwrap();
    union.sort();
    assert_eq!(
        union,
        vec![b"w".to_vec(), b"x".to_vec(), b"y".to_vec(), b"z".to_vec()]
    );

    let mut diff = c.sdiff(&[&b"a"[..], &b"b"[..]]).unwrap();
    diff.sort();
    assert_eq!(diff, vec![b"x".to_vec()]);

    // Empty input → empty output (no panic).
    assert!(c.sinter(&[]).unwrap().is_empty());
}
