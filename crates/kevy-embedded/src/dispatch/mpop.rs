//! `ZMPOP` / `LMPOP`. The keys may live on different shards, so each is
//! tried in order as the single-key form, under its own shard's lock; the
//! first that gives something answers. Not atomic across shards, as on
//! the server.

use kevy_resp::encode_error;
use kevy_verbs::mpop::{parse_lmpop, parse_zmpop};
use kevy_verbs::reply::wrong_args;

use super::{Args, verb_name};
use crate::store::Store;

const NIL: &[u8] = b"*-1\r\n";

/// One multi-key pop; `false` = verb not in this group.
pub(super) fn dispatch(s: &Store, up: &[u8], argv: &[Vec<u8>], out: &mut Vec<u8>) -> bool {
    let zset = match up {
        b"ZMPOP" => true,
        b"LMPOP" => false,
        _ => return false,
    };
    if argv.len() < 4 {
        wrong_args(out, &verb_name(argv));
        return true;
    }
    let args = Args::new(argv);
    let parsed = if zset { parse_zmpop(&args, 1) } else { parse_lmpop(&args, 1) };
    let p = match parsed {
        Ok(p) => p,
        Err(e) => {
            encode_error(out, e.as_wire());
            return true;
        }
    };
    let end = &argv[2 + p.numkeys];
    let count = p.count.to_string().into_bytes();
    for key in &argv[2..2 + p.numkeys] {
        let one = [
            argv[0].clone(),
            b"1".to_vec(),
            key.clone(),
            end.clone(),
            b"COUNT".to_vec(),
            count.clone(),
        ];
        let mark = out.len();
        super::shared::dispatch(s, up, &one, out);
        if out[mark..] != *NIL {
            return true;
        }
        out.truncate(mark);
    }
    out.extend_from_slice(NIL);
    true
}
