//! Where `feed follow` starts and what it remembers: its arguments, each
//! shard's first cursor, and the checkpoint file (`shard generation offset`
//! per line, rewritten after every round).

use super::feed::Cursor;
use crate::rcli::session::{Session, eprint_bytes};
use kevy_resp::Reply;

/// The follow's settings.
pub(crate) struct Plan {
    pub(crate) prefixes: Vec<Vec<u8>>,
    pub(crate) shard: Option<u64>,
    pub(crate) from: Option<(i64, i64)>,
    pub(crate) checkpoint: Option<std::path::PathBuf>,
    pub(crate) resp: bool,
    pub(crate) jump: bool,
    pub(crate) limit: Option<u64>,
}

/// `follow` and its options.
pub(crate) fn parse(args: &[Vec<u8>]) -> Option<Plan> {
    let Some((verb, rest)) = args.split_first().filter(|(v, _)| v.as_slice() == b"follow") else {
        return usage(args.first().map_or(&b""[..], Vec::as_slice));
    };
    let _ = verb; // only `follow` exists
    let mut plan = Plan {
        prefixes: Vec::new(),
        shard: None,
        from: None,
        checkpoint: None,
        resp: false,
        jump: false,
        limit: None,
    };
    for pair in rest.chunks(2) {
        let [flag, value] = pair else { return usage(&pair[0]) };
        let number = || std::str::from_utf8(value).ok()?.parse::<u64>().ok();
        match flag.as_slice() {
            b"--prefix" => plan.prefixes.push(value.clone()),
            b"--shard" if value == b"all" => plan.shard = None,
            b"--shard" => plan.shard = Some(number().or_else(|| usage(value))?),
            b"--from" if value == b"tail" => plan.from = None,
            b"--from" => plan.from = Some(position(value).or_else(|| usage(value))?),
            b"--checkpoint" => {
                plan.checkpoint = Some(String::from_utf8_lossy(value).into_owned().into())
            }
            b"--as" => {
                plan.resp = match value.as_slice() {
                    b"resp" => true,
                    b"json" => false,
                    _ => return usage(value),
                }
            }
            b"--on-resync" => {
                plan.jump = match value.as_slice() {
                    b"jump" => true,
                    b"stop" => false,
                    _ => return usage(value),
                }
            }
            b"--limit" => plan.limit = Some(number().or_else(|| usage(value))?),
            other => return usage(other),
        }
    }
    if plan.from.is_some() && plan.shard.is_none() {
        eprint_bytes(&[b"kevy-cli: feed follow: --from gen:offset needs --shard n\n"]);
        return None;
    }
    Some(plan)
}

fn position(text: &[u8]) -> Option<(i64, i64)> {
    let colon = text.iter().position(|&b| b == b':')?;
    let n = |t: &[u8]| std::str::from_utf8(t).ok()?.parse::<i64>().ok();
    Some((n(&text[..colon])?, n(&text[colon + 1..])?))
}

fn usage<T>(bad: &[u8]) -> Option<T> {
    if !bad.is_empty() {
        eprint_bytes(&[b"kevy-cli: feed: unexpected '", bad, b"'\n"]);
    }
    eprint_bytes(&[b"usage: kevy-cli feed follow [--prefix p]... [--shard n|all] [--from tail|gen:offset] [--checkpoint file] [--as json|resp] [--on-resync stop|jump] [--limit n]\n"]);
    None
}

/// Each followed shard's first cursor: the checkpoint's, `--from`, or the
/// shard's tail now.
pub(crate) fn start(s: &mut Session, plan: &Plan) -> Option<Vec<Cursor>> {
    let saved = plan
        .checkpoint
        .as_ref()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|t| checkpoint_cursors(&t))
        .unwrap_or_default();
    let shards: Vec<u64> = match plan.shard {
        Some(n) => vec![n],
        None => match s.request(&[b"FEED.SHARDS"]) {
            Ok(Reply::Int(n)) => (0..n.max(0) as u64).collect(),
            _ => {
                eprint_bytes(&[
                    b"kevy-cli: feed follow: FEED.SHARDS did not answer a shard count\n",
                ]);
                return None;
            }
        },
    };
    shards.into_iter().map(|shard| first_cursor(s, plan, &saved, shard)).collect()
}

fn first_cursor(s: &mut Session, plan: &Plan, saved: &[Cursor], shard: u64) -> Option<Cursor> {
    if let Some(c) = saved.iter().find(|c| c.shard == shard) {
        return Some(c.clone());
    }
    if let Some((generation, offset)) = plan.from {
        return Some(Cursor { shard, generation, offset });
    }
    let number = shard.to_string();
    match s.request(&[b"FEED.TAIL", number.as_bytes()]) {
        Ok(Reply::Array(t)) => match (t.first(), t.get(1)) {
            (Some(Reply::Int(generation)), Some(Reply::Int(offset))) => {
                Some(Cursor { shard, generation: *generation, offset: *offset })
            }
            _ => None,
        },
        Ok(Reply::Error(msg)) => {
            eprint_bytes(&[b"(error) ", &msg, b"\n"]);
            None
        }
        _ => None,
    }
}

fn checkpoint_cursors(text: &str) -> Vec<Cursor> {
    text.lines()
        .filter_map(|l| {
            let mut w = l.split_whitespace();
            Some(Cursor {
                shard: w.next()?.parse().ok()?,
                generation: w.next()?.parse().ok()?,
                offset: w.next()?.parse().ok()?,
            })
        })
        .collect()
}

/// Rewrite the checkpoint file, if there is one.
pub(crate) fn save(plan: &Plan, cursors: &[Cursor]) {
    let Some(path) = &plan.checkpoint else { return };
    let text: String =
        cursors.iter().map(|c| format!("{} {} {}\n", c.shard, c.generation, c.offset)).collect();
    let tmp = path.with_extension("tmp");
    if std::fs::write(&tmp, text).and_then(|()| std::fs::rename(&tmp, path)).is_err() {
        eprint_bytes(&[b"kevy-cli: feed follow: cannot write the checkpoint file\n"]);
    }
}

#[cfg(test)]
mod tests {
    use super::{checkpoint_cursors, parse};
    use crate::rcli::rds::feed::Cursor;

    fn args(a: &[&str]) -> Vec<Vec<u8>> {
        a.iter().map(|s| s.as_bytes().to_vec()).collect()
    }

    #[test]
    fn follow_options_and_checkpoints() {
        let p = parse(&args(&[
            "follow", "--prefix", "user:", "--shard", "1", "--from", "7:42", "--as", "resp",
            "--limit", "5",
        ]))
        .unwrap();
        assert_eq!(
            (p.prefixes.len(), p.shard, p.from, p.resp, p.limit),
            (1, Some(1), Some((7, 42)), true, Some(5))
        );
        assert!(parse(&args(&["follow", "--from", "7:42"])).is_none());
        assert!(parse(&args(&["tail"])).is_none());
        assert!(parse(&args(&["follow", "--as"])).is_none());
        assert_eq!(
            checkpoint_cursors("0 9 3\nbad\n1 9 4\n"),
            vec![
                Cursor { shard: 0, generation: 9, offset: 3 },
                Cursor { shard: 1, generation: 9, offset: 4 }
            ]
        );
    }
}
