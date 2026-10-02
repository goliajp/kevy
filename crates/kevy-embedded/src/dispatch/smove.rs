//! `SMOVE` whose two keys live on different shards: the destination's
//! type, then the source's checks and the removal, then the add, each
//! under its own shard's lock — Redis's order of checks, kept across
//! shards; the move is not atomic across them, as on the server. Keys on
//! one shard take the shared command under that shard's lock.

use kevy_resp::{encode_error, encode_integer};

use crate::KevyResult;
use crate::store::{Store, commit_write};

const WRONGTYPE: &str = "WRONGTYPE Operation against a key holding the wrong kind of value";

/// `SMOVE`; `false` = verb not in this group.
pub(super) fn dispatch(s: &Store, up: &[u8], argv: &[Vec<u8>], out: &mut Vec<u8>) -> bool {
    if up != b"SMOVE" {
        return false;
    }
    let n = s.shards.len();
    let apart = argv.len() == 4
        && crate::shard::shard_idx(&argv[1], n) != crate::shard::shard_idx(&argv[2], n);
    if !apart {
        return super::shared::dispatch(s, up, argv, out);
    }
    match smove_apart(s, &argv[1], &argv[2], &argv[3]) {
        Ok(Some(moved)) => encode_integer(out, i64::from(moved)),
        Ok(None) => encode_error(out, WRONGTYPE),
        Err(e) => super::kevy_err(out, &e),
    }
    true
}

/// `Some(moved)`, or `None` for WRONGTYPE.
fn smove_apart(s: &Store, src: &[u8], dst: &[u8], member: &[u8]) -> KevyResult<Option<bool>> {
    let dst_is_set = s.wshard(dst).store.scard(dst).is_ok();
    {
        let mut g = s.wshard(src);
        match g.store.scard(src) {
            Ok(0) => return Ok(Some(false)),
            Err(_) => return Ok(None),
            Ok(_) if !dst_is_set => return Ok(None),
            Ok(_) => {}
        }
        if g.store.srem(src, &[member]).ok() != Some(1) {
            return Ok(Some(false));
        }
        commit_write(&mut g, &[b"SREM", src, member])?;
    }
    let mut g = s.wshard(dst);
    match g.store.sadd(dst, &[member]) {
        Ok(added) => {
            if added > 0 {
                commit_write(&mut g, &[b"SADD", dst, member])?;
            }
            Ok(Some(true))
        }
        Err(_) => {
            drop(g);
            // the destination stopped being a set: the member goes back
            let mut g = s.wshard(src);
            g.store.sadd(src, &[member]).ok();
            commit_write(&mut g, &[b"SADD", src, member])?;
            Ok(None)
        }
    }
}
