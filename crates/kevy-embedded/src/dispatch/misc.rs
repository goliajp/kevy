//! Connection-face verbs the embedded engine can honestly serve
//! (PING / ECHO / PUBLISH), the migration digests (PREFIX.*), and the
//! CDC feed consumer verbs (FEED.*, `replicate` builds only — the
//! embedded feed is a single stream, so the argv shapes follow the
//! embedded listener precedent, not the server's per-shard forms).

use crate::store::Store;

use super::util::{arr, bulk, err, int, simple, wrong_args};

/// One conn/digest/feed request; `false` = verb not in this group.
// LOC-WAIVER: data-driven verb dispatch table — one reply-emitter arm per verb.
pub(super) fn dispatch(s: &Store, up: &[u8], argv: &[Vec<u8>], out: &mut Vec<u8>) -> bool {
    match up {
        b"PING" => match argv.len() {
            1 => simple(out, "PONG"),
            2 => bulk(out, &argv[1]),
            _ => wrong_args(out, "ping"),
        },
        b"ECHO" => {
            if argv.len() == 2 {
                bulk(out, &argv[1]);
            } else {
                wrong_args(out, "echo");
            }
        }
        b"PUBLISH" => {
            if argv.len() == 3 {
                int(out, s.publish(&argv[1], &argv[2]) as i64);
            } else {
                wrong_args(out, "publish");
            }
        }
        b"PREFIX.DIGEST" => {
            if argv.len() == 2 {
                let (count, xor) = s.prefix_digest(&argv[1]);
                arr(out, 2);
                int(out, count as i64);
                bulk(out, format!("{xor:016x}").as_bytes());
            } else {
                err(out, "ERR bad PREFIX.DIGEST arguments");
            }
        }
        #[cfg(all(feature = "replicate", not(target_arch = "wasm32")))]
        b"PREFIX.STATS" => {
            if argv.len() == 2 {
                let info = s.info_prefix(&argv[1]);
                out.extend_from_slice(
                    format!(
                        "*4\r\n$4\r\nkeys\r\n:{}\r\n$7\r\nexpires\r\n:{}\r\n",
                        info.keys, info.expires
                    )
                    .as_bytes(),
                );
            } else {
                err(out, "ERR bad PREFIX.STATS arguments");
            }
        }
        #[cfg(all(feature = "replicate", not(target_arch = "wasm32")))]
        b"FEED.SHARDS" => {
            if argv.len() == 1 {
                int(out, s.feed_shards() as i64);
            } else {
                wrong_args(out, "feed.shards");
            }
        }
        #[cfg(all(feature = "replicate", not(target_arch = "wasm32")))]
        b"FEED.TAIL" => {
            if argv.len() != 1 {
                wrong_args(out, "feed.tail");
            } else {
                match s.changes_tail() {
                    Ok((g, o)) => {
                        arr(out, 2);
                        int(out, g as i64);
                        int(out, o as i64);
                    }
                    Err(e) => err(out, &format!("ERR feed: {e:?}")),
                }
            }
        }
        #[cfg(all(feature = "replicate", not(target_arch = "wasm32")))]
        b"FEED.READ" => {
            if argv.len() >= 4 {
                cmd_feed_read(s, argv, out);
            } else {
                err(out, "ERR FEED.READ gen offset limit [PREFIX p…]");
            }
        }
        _ => return false,
    }
    true
}

/// `FEED.READ gen offset limit [PREFIX p…]` — the embedded listener's
/// wire shape (`listener/verbs.rs::cmd_feed_read`), byte for byte.
#[cfg(all(feature = "replicate", not(target_arch = "wasm32")))]
fn cmd_feed_read(s: &Store, argv: &[Vec<u8>], out: &mut Vec<u8>) {
    use super::util::arg_i64;
    let (Some(g), Some(o), Some(limit)) =
        (arg_i64(&argv[1]), arg_i64(&argv[2]), arg_i64(&argv[3]))
    else {
        return err(out, "ERR FEED.READ gen offset limit [PREFIX p…]");
    };
    let mut prefixes: Vec<&[u8]> = Vec::new();
    if argv.len() > 4 {
        if !argv[4].eq_ignore_ascii_case(b"PREFIX") || argv.len() < 6 {
            return err(out, "ERR FEED.READ gen offset limit [PREFIX p…]");
        }
        prefixes = argv[5..].iter().map(Vec::as_slice).collect();
    }
    match s.changes_since(g as u64, o as u64, limit.clamp(1, 10_000) as usize, &prefixes) {
        Ok(batch) => {
            arr(out, 3);
            int(out, batch.next.0 as i64);
            int(out, batch.next.1 as i64);
            arr(out, batch.changes.len());
            for f in &batch.changes {
                arr(out, f.argv.len());
                for a in &f.argv {
                    bulk(out, a);
                }
            }
        }
        Err(e) => err(out, &format!("ERR feed: {e:?}")),
    }
}
