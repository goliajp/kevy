//! `LCS`, `SINTERCARD`, `ZINTER` / `ZUNION` / `ZDIFF`: reads over several
//! keys that may live on different shards. Each key is copied under its
//! own shard's lock and the command runs over the copies, as on the
//! server; the answer is not a point-in-time snapshot across shards.
//! `ZRANGESTORE` and `SORT … STORE` read their source the same way and
//! then write their destination under that key's lock.

use kevy_verbs::multikey::{parse_zcombine, parse_zdiff, parse_zintercard};

use super::Args;
use crate::store::Store;

/// One multi-key read; `false` = verb not in this group.
pub(super) fn dispatch(s: &Store, up: &[u8], argv: &[Vec<u8>], out: &mut Vec<u8>) -> bool {
    if up == b"ZRANGESTORE" {
        zrangestore(s, argv, out);
        return true;
    }
    if up == b"SORT" && kevy_verbs::sort::store_destination(&Args::new(argv)).is_some() {
        super::sort_store::sort_store(s, argv, out);
        return true;
    }
    if !matches!(up, b"LCS" | b"SINTERCARD" | b"ZINTER" | b"ZUNION" | b"ZDIFF") {
        return false;
    }
    let args = Args::new(argv);
    // a malformed call reaches the command over no copies and is refused there
    let keys = match up {
        _ if argv.len() < 3 => 0..0,
        b"LCS" => 1..3,
        b"SINTERCARD" => parse_zintercard(&args).map_or(0..0, |(n, _)| 2..2 + n),
        b"ZDIFF" => parse_zdiff(&args).map_or(0..0, |p| 2..2 + p.numkeys),
        _ => parse_zcombine(&args).map_or(0..0, |p| 2..2 + p.numkeys),
    };
    let mut copies = kevy_store::Store::new();
    for key in &argv[keys] {
        let copy = s.wshard(key).store.clone_with_ttl(key);
        if let Some((value, ttl_ms)) = copy {
            copies.put_with_ttl(key.clone(), value, ttl_ms);
        }
    }
    kevy_verbs::exec(&mut copies, up, &args, out);
    true
}

/// `ZRANGESTORE dst src …`: the range of a copy of `src`, stored at `dst`
/// and recorded as the `DEL` and `ZADD` that rebuild it.
fn zrangestore(s: &Store, argv: &[Vec<u8>], out: &mut Vec<u8>) {
    let args = Args::new(argv);
    let mut copies = kevy_store::Store::new();
    if argv.len() >= 5
        && let Some((value, ttl_ms)) = s.wshard(&argv[2]).store.clone_with_ttl(&argv[2])
    {
        copies.put_with_ttl(argv[2].clone(), value, ttl_ms);
    }
    let mark = out.len();
    kevy_verbs::exec(&mut copies, b"ZRANGESTORE", &args, out);
    if argv.len() < 5 || out.get(mark) == Some(&b'-') {
        return;
    }
    let dst = &argv[1];
    let items = copies.zrange(dst, 0, -1).unwrap_or_default();
    let mut g = s.wshard(dst);
    if items.is_empty() && !g.store.key_exists(dst) {
        return;
    }
    g.store.zstore_result(dst, &items);
    let scores: Vec<Vec<u8>> =
        items.iter().map(|(_, sc)| kevy_verbs::reply::fmt_score(*sc)).collect();
    let mut zadd: Vec<&[u8]> = vec![b"ZADD", dst];
    for ((m, _), sc) in items.iter().zip(&scores) {
        zadd.push(sc);
        zadd.push(m);
    }
    let recorded = crate::store::commit_write(&mut g, &[b"DEL", dst]).and_then(|()| {
        if items.is_empty() { Ok(()) } else { crate::store::commit_write(&mut g, &zadd) }
    });
    if let Err(e) = recorded {
        out.truncate(mark);
        super::kevy_err(out, &e);
    }
}
