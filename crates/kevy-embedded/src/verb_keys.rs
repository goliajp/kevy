//! The key that picks a command's shard.
//!
//! A command's key is its first argument, except in the stream family:
//! `XGROUP` and `XINFO` name a subcommand first, and `XREAD` /
//! `XREADGROUP` name their streams after `STREAMS`. Routing those by
//! the first argument would run them on a shard that does not hold the
//! stream.

use std::ops::Range;

use kevy_resp::ArgvView;

/// The key whose shard runs `args`; `up` is its verb, uppercase.
pub(crate) fn shard_key<'a, A: ArgvView + ?Sized>(up: &[u8], args: &'a A) -> Option<&'a [u8]> {
    if up == b"ZMPOP" || up == b"LMPOP" {
        // the single-key form `ZMPOP 1 key …` is all that reaches a shard
        return args.get(2);
    }
    if !kevy_verbs::is_streams_geo(up) {
        return args.get(1);
    }
    match up {
        b"XGROUP" | b"XINFO" => args.get(2),
        b"XREAD" | b"XREADGROUP" => {
            stream_keys(up, args).and_then(|r| args.get(r.start)).or_else(|| args.get(1))
        }
        _ => args.get(1),
    }
}

/// Where the stream keys of an `XREAD` / `XREADGROUP` sit: the first
/// half of what follows `STREAMS`. `None` when the call is malformed,
/// which the command itself refuses.
pub(crate) fn stream_keys<A: ArgvView + ?Sized>(up: &[u8], args: &A) -> Option<Range<usize>> {
    let at = (options_from(up)..args.len()).find(|&i| args[i].eq_ignore_ascii_case(b"STREAMS"))?;
    let rest = args.len() - at - 1;
    if rest == 0 || !rest.is_multiple_of(2) {
        return None;
    }
    Some(at + 1..at + 1 + rest / 2)
}

/// Whether an `XREAD` / `XREADGROUP` asks to block: `BLOCK` among the
/// options before `STREAMS`.
#[cfg(any(test, feature = "streams-geo"))]
pub(crate) fn blocks<A: ArgvView + ?Sized>(up: &[u8], args: &A) -> bool {
    if up != b"XREAD" && up != b"XREADGROUP" {
        return false;
    }
    (options_from(up)..args.len())
        .take_while(|&i| !args[i].eq_ignore_ascii_case(b"STREAMS"))
        .any(|i| args[i].eq_ignore_ascii_case(b"BLOCK"))
}

/// The first option of a stream read: after `GROUP group consumer` for
/// `XREADGROUP`.
fn options_from(up: &[u8]) -> usize {
    if up == b"XREADGROUP" { 4 } else { 1 }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(s: &str) -> kevy_resp::Argv {
        kevy_resp::Argv::from(s.split(' ').map(|p| p.as_bytes().to_vec()).collect::<Vec<_>>())
    }

    fn up(a: &kevy_resp::Argv) -> Vec<u8> {
        let mut buf = [0u8; 32];
        kevy_verbs::args::upper_verb(&a[0], &mut buf).to_vec()
    }

    fn key(s: &str) -> Option<Vec<u8>> {
        let a = argv(s);
        shard_key(&up(&a), &a).map(<[u8]>::to_vec)
    }

    #[test]
    fn the_stream_family_is_keyed_by_its_stream() {
        let k = |s| key(s).map(|k| String::from_utf8(k).unwrap());
        assert_eq!(k("SET k v").as_deref(), Some("k"));
        assert_eq!(k("XADD s * f v").as_deref(), Some("s"));
        assert_eq!(k("XGROUP CREATE s g 0").as_deref(), Some("s"));
        assert_eq!(k("XINFO STREAM s").as_deref(), Some("s"));
        assert_eq!(k("XREAD COUNT 2 STREAMS a b 0 0").as_deref(), Some("a"));
        assert_eq!(k("XREADGROUP GROUP STREAMS c STREAMS s >").as_deref(), Some("s"));
        // malformed: the command refuses it on whatever shard it lands
        assert_eq!(k("XREAD STREAMS a b 0").as_deref(), Some("STREAMS"));
        assert_eq!(k("XGROUP HELP"), None);
    }

    #[test]
    fn only_a_block_option_blocks() {
        let b = |s: &str| {
            let a = argv(s);
            blocks(&up(&a), &a)
        };
        assert!(b("XREAD BLOCK 0 STREAMS s $"));
        assert!(b("xreadgroup GROUP g c block 10 STREAMS s >"));
        assert!(!b("XREADGROUP GROUP BLOCK c STREAMS s >"));
        assert!(!b("XREAD STREAMS BLOCK 0"));
        assert!(!b("XADD BLOCK * f v"));
    }
}
