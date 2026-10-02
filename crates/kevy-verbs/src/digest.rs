//! `DIGEST key`: the XXH3 hash of a string value's bytes, as Redis gives
//! it — 16 lowercase hex digits. `SET … IFDEQ` compares against the same.

use kevy_resp::{ArgvView, encode_bulk, encode_null_bulk};
use kevy_store::Store;

use crate::Effect;
use crate::reply::{store_err, wrong_args};

/// The digest of `value`, as 16 lowercase hex digits.
pub(crate) fn hex(value: &[u8]) -> [u8; 16] {
    let h = kevy_hash::xxh3_64(value);
    let mut out = [0u8; 16];
    for (i, b) in out.iter_mut().enumerate() {
        *b = b"0123456789abcdef"[(h >> (60 - 4 * i) & 0xf) as usize];
    }
    out
}

pub(crate) fn digest<A: ArgvView + ?Sized>(store: &mut Store, args: &A, out: &mut Vec<u8>) {
    if args.len() != 2 {
        return wrong_args(out, "digest");
    }
    match store.get(&args[1]) {
        Ok(Some(v)) => encode_bulk(out, &hex(&v)),
        Ok(None) => encode_null_bulk(out),
        Err(e) => store_err(out, e),
    }
}

/// The digest the verb answers, for an `Effect`-returning caller.
pub(crate) fn exec<A: ArgvView + ?Sized>(store: &mut Store, args: &A, out: &mut Vec<u8>) -> Effect {
    digest(store, args, out);
    Effect::Read
}
