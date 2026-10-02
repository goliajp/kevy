//! `PFADD`, `PFCOUNT`, `PFMERGE` over the keys in one store.

use kevy_resp::{ArgvView, encode_integer, encode_simple_string};
use kevy_store::Store;

use crate::args::rest_borrowed;
use crate::reply::{store_err, wrong_args};
use crate::{Effect, changed};

/// One command of this group; `None` = not in the group.
pub(crate) fn exec<A: ArgvView + ?Sized>(
    cmd: &[u8],
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
) -> Option<Effect> {
    let name = match cmd {
        b"PFADD" => "pfadd",
        b"PFCOUNT" => "pfcount",
        b"PFMERGE" => "pfmerge",
        _ => return None,
    };
    if args.len() < 2 {
        wrong_args(out, name);
        return Some(Effect::Unchanged);
    }
    let result = match cmd {
        b"PFADD" => store.pfadd(&args[1], &rest_borrowed(args, 2)).map(|added| {
            encode_integer(out, i64::from(added));
            changed(added)
        }),
        // writing a key's cached estimate changes its bytes, so it is a write
        b"PFCOUNT" => store.pfcount(&rest_borrowed(args, 1)).map(|(card, cached)| {
            encode_integer(out, card as i64);
            if cached { Effect::Write } else { Effect::Read }
        }),
        _ => store.pfmerge(&args[1], &rest_borrowed(args, 2)).map(|()| {
            encode_simple_string(out, "OK");
            Effect::Write
        }),
    };
    Some(result.unwrap_or_else(|e| {
        store_err(out, e);
        Effect::Unchanged
    }))
}
