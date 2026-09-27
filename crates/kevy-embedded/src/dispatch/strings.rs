//! The string verbs that stay on the facade: `GET` for its lock policy,
//! and the multi-key `MGET` / `MSET`, which span shards. Every other
//! string verb runs through `shared`.

use crate::store::Store;

use super::{kevy_err, opt_bulk, rest};
use kevy_resp::{encode_array_len, encode_simple_string};
use kevy_verbs::reply::wrong_args;

/// One of these requests; `false` = verb not in this group.
pub(super) fn dispatch(s: &Store, up: &[u8], argv: &[Vec<u8>], out: &mut Vec<u8>) -> bool {
    match up {
        // the facade's read takes the shard's shared lock whenever the
        // eviction policy allows; the shared layer needs the exclusive one
        b"GET" => match argv.len() {
            2 => match s.get(&argv[1]) {
                Ok(v) => opt_bulk(out, v),
                Err(e) => kevy_err(out, &e),
            },
            _ => wrong_args(out, "get"),
        },
        b"MGET" => {
            if argv.len() < 2 {
                wrong_args(out, "mget");
            } else {
                match s.mget(&rest(argv, 1)) {
                    Ok(vals) => {
                        encode_array_len(out, vals.len() as i64);
                        for v in vals {
                            opt_bulk(out, v);
                        }
                    }
                    Err(e) => kevy_err(out, &e),
                }
            }
        }
        b"MSET" => {
            if argv.len() < 3 || argv.len().is_multiple_of(2) {
                wrong_args(out, "mset");
            } else {
                let pairs: Vec<(&[u8], &[u8])> = (1..argv.len())
                    .step_by(2)
                    .map(|i| (argv[i].as_slice(), argv[i + 1].as_slice()))
                    .collect();
                match s.mset(&pairs) {
                    Ok(()) => encode_simple_string(out, "OK"),
                    Err(e) => kevy_err(out, &e),
                }
            }
        }
        _ => return false,
    }
    true
}
