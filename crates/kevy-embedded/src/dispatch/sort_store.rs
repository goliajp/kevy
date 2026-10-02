//! `SORT … STORE dst` and `PFMERGE dst src…`: computed over copies of
//! their keys, the result placed at `dst` under its own shard's lock and
//! recorded as the writes that rebuild it there.

use super::Args;
use crate::store::Store;

pub(super) fn sort_store(s: &Store, argv: &[Vec<u8>], out: &mut Vec<u8>) {
    let args = Args::new(argv);
    let Some(at) = kevy_verbs::sort::store_destination(&args) else { return };
    let mut copies = kevy_store::Store::new();
    if let Some((value, ttl_ms)) = s.wshard(&argv[1]).store.clone_with_ttl(&argv[1]) {
        copies.put_with_ttl(argv[1].clone(), value, ttl_ms);
    }
    let mark = out.len();
    kevy_verbs::exec(&mut copies, b"SORT", &args, out);
    if out.get(mark) == Some(&b'-') {
        return;
    }
    let dst = &argv[at];
    let items = copies.lrange(dst, 0, -1).unwrap_or_default();
    let mut g = s.wshard(dst);
    if items.is_empty() && !g.store.key_exists(dst) {
        return;
    }
    g.store.del(&[dst]);
    let rows: Vec<&[u8]> = items.iter().map(Vec::as_slice).collect();
    if !rows.is_empty() {
        g.store.rpush(dst, &rows).expect("a removed key takes a list");
    }
    let mut rpush: Vec<&[u8]> = vec![b"RPUSH", dst];
    rpush.extend(&rows);
    let recorded = crate::store::commit_write(&mut g, &[b"DEL", dst]).and_then(|()| {
        if rows.is_empty() { Ok(()) } else { crate::store::commit_write(&mut g, &rpush) }
    });
    if let Err(e) = recorded {
        out.truncate(mark);
        super::kevy_err(out, &e);
    }
}

/// `PFMERGE dst src…`: merged over copies of every key, `dst` included,
/// and placed back keeping `dst`'s deadline; recorded as the `SET` of the
/// merged bytes and that deadline.
pub(super) fn pfmerge(s: &Store, argv: &[Vec<u8>], out: &mut Vec<u8>) {
    let args = Args::new(argv);
    let mut copies = kevy_store::Store::new();
    for key in &argv[1..] {
        if let Some((value, ttl_ms)) = s.wshard(key).store.clone_with_ttl(key) {
            copies.put_with_ttl(key.clone(), value, ttl_ms);
        }
    }
    let mark = out.len();
    kevy_verbs::exec(&mut copies, b"PFMERGE", &args, out);
    if out.get(mark) == Some(&b'-') {
        return;
    }
    let dst = &argv[1];
    let Some(bytes) = copies.get(dst).ok().flatten().map(|b| b.into_owned()) else { return };
    let Some((value, _)) = copies.clone_with_ttl(dst) else { return };
    let mut g = s.wshard(dst);
    g.store.put_keep_ttl(dst.clone(), value);
    let recorded = crate::store::commit_write(&mut g, &[b"SET", dst, &bytes])
        .and_then(|()| crate::store_glue::commit_deadline(&mut g, dst));
    if let Err(e) = recorded {
        out.truncate(mark);
        super::kevy_err(out, &e);
    }
}
