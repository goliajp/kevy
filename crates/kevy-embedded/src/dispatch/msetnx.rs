//! `MSETNX`: each key is looked up under its own shard's lock, and only
//! when none exists are the pairs set, shard by shard — not atomic across
//! shards, as on the server.

use kevy_resp::encode_integer;

use crate::store::Store;

/// `MSETNX`; `false` = verb not in this group.
pub(super) fn dispatch(s: &Store, up: &[u8], argv: &[Vec<u8>], out: &mut Vec<u8>) -> bool {
    if up != b"MSETNX" {
        return false;
    }
    if argv.len() < 3 || argv.len().is_multiple_of(2) {
        return super::shared::dispatch(s, up, argv, out);
    }
    if argv[1..].iter().step_by(2).any(|k| s.wshard(k).store.key_exists(k)) {
        encode_integer(out, 0);
        return true;
    }
    let pairs: Vec<(&[u8], &[u8])> =
        argv[1..].chunks(2).map(|p| (p[0].as_slice(), p[1].as_slice())).collect();
    match s.mset(&pairs) {
        Ok(()) => encode_integer(out, 1),
        Err(e) => super::kevy_err(out, &e),
    }
    true
}
