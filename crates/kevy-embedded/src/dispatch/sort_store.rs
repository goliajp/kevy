//! `SORT … STORE dst`: sorted from a copy of the source, the list placed
//! at `dst` under its own shard's lock and recorded as the `DEL` and
//! `RPUSH` that rebuild it.

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
