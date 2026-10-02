//! `LCS`, `SINTERCARD`, `ZINTER` / `ZUNION` / `ZDIFF`: reads over several
//! keys that may live on different shards. Each key is copied under its
//! own shard's lock and the command runs over the copies, as on the
//! server; the answer is not a point-in-time snapshot across shards.

use kevy_verbs::multikey::{parse_zcombine, parse_zdiff, parse_zintercard};

use super::Args;
use crate::store::Store;

/// One multi-key read; `false` = verb not in this group.
pub(super) fn dispatch(s: &Store, up: &[u8], argv: &[Vec<u8>], out: &mut Vec<u8>) -> bool {
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
