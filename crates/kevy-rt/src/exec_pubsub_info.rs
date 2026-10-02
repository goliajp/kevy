//! `PUBSUB` introspection over the registries every shard shares. kevy
//! has no shard channels (`SSUBSCRIBE`), so the two `SHARD*` questions
//! answer as Redis does for a server with none.

use kevy_resp::{ArgvView, encode_array_len, encode_bulk, encode_error, encode_integer};
use kevy_store::glob_match;

use crate::Commands;
use crate::shard::Shard;

const HELP: &[&str] = &[
    "PUBSUB <subcommand> [<arg> [value] [opt] ...]. Subcommands are:",
    "CHANNELS [<pattern>]",
    "    Return the currently active channels matching a <pattern> (default: '*').",
    "NUMPAT",
    "    Return number of subscriptions to patterns.",
    "NUMSUB [<channel> ...]",
    "    Return the number of subscribers for the specified channels, excluding",
    "    pattern subscriptions(default: no channels).",
    "SHARDCHANNELS [<pattern>]",
    "    Return the currently active shard level channels matching a <pattern> (default: '*').",
    "SHARDNUMSUB [<shardchannel> ...]",
    "    Return the number of subscribers for the specified shard level channel(s)",
    "HELP",
    "    Print this help.",
];

impl<C: Commands> Shard<C> {
    /// The reply to `PUBSUB …`.
    pub(crate) fn pubsub_info<A: ArgvView + ?Sized>(&self, args: &A) -> Vec<u8> {
        let mut out = Vec::new();
        if args.len() < 2 {
            encode_error(&mut out, "ERR wrong number of arguments for 'pubsub' command");
            return out;
        }
        let given = String::from_utf8_lossy(&args[1]).into_owned();
        match given.to_ascii_uppercase().as_str() {
            "CHANNELS" if args.len() <= 3 => self.channels(args.get(2), &mut out),
            "SHARDCHANNELS" if args.len() <= 3 => encode_array_len(&mut out, 0),
            "NUMSUB" => {
                let reg = self.pubsub.read().expect("pubsub registry");
                numsub(args, &mut out, |ch| reg.get(ch).map_or(0, |e| e.0));
            }
            "SHARDNUMSUB" => numsub(args, &mut out, |_| 0),
            "NUMPAT" if args.len() == 2 => {
                let reg = self.pubsub_patterns.read().expect("pattern registry");
                encode_integer(&mut out, reg.iter().filter(|e| e.1 > 0).count() as i64);
            }
            "NUMPAT" => {
                encode_error(&mut out, "ERR wrong number of arguments for 'pubsub|numpat' command")
            }
            "HELP" if args.len() == 2 => help(&mut out),
            "CHANNELS" | "SHARDCHANNELS" | "HELP" => encode_error(
                &mut out,
                &format!(
                    "ERR unknown subcommand or wrong number of arguments for '{given}'. Try PUBSUB HELP."
                ),
            ),
            _ => encode_error(
                &mut out,
                &format!("ERR unknown subcommand '{given}'. Try PUBSUB HELP."),
            ),
        }
        out
    }

    /// The channels with a subscriber, matching `pattern` (all without one).
    fn channels(&self, pattern: Option<&[u8]>, out: &mut Vec<u8>) {
        let pattern = pattern.unwrap_or(b"*");
        let reg = self.pubsub.read().expect("pubsub registry");
        let names: Vec<&Vec<u8>> = reg
            .iter()
            .filter(|(ch, (n, _))| *n > 0 && glob_match(pattern, ch))
            .map(|(ch, _)| ch)
            .collect();
        encode_array_len(out, names.len() as i64);
        for ch in names {
            encode_bulk(out, ch);
        }
    }
}

/// Each channel named after the subcommand, with its count.
fn numsub<A: ArgvView + ?Sized>(args: &A, out: &mut Vec<u8>, count: impl Fn(&[u8]) -> u32) {
    encode_array_len(out, ((args.len() - 2) * 2) as i64);
    for i in 2..args.len() {
        encode_bulk(out, &args[i]);
        encode_integer(out, i64::from(count(&args[i])));
    }
}

fn help(out: &mut Vec<u8>) {
    encode_array_len(out, HELP.len() as i64);
    for line in HELP {
        out.extend_from_slice(b"+");
        out.extend_from_slice(line.as_bytes());
        out.extend_from_slice(b"\r\n");
    }
}
