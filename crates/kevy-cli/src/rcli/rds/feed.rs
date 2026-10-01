//! `feed follow [--prefix p]… [--shard n|all] [--from tail|gen:offset]
//! [--checkpoint file] [--as json|resp] [--on-resync stop|jump]
//! [--limit n]`: the change feed, every shard polled in turn.
//!
//! There is no order across shards and no server-side consumer position:
//! the cursor lives here, and in the checkpoint file when one is given.

use super::feed_start::{Plan, parse, save, start};
use super::options::Common;
use crate::rcli::send::write_out;
use crate::rcli::session::{Session, eprint_bytes};
use kevy_resp::Reply;
use std::time::Duration;

/// Where one shard is read from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Cursor {
    pub(crate) shard: u64,
    pub(crate) generation: i64,
    pub(crate) offset: i64,
}

/// Run `feed follow`; the exit code (3 when a shard needs a resync and
/// `--on-resync stop`).
pub(crate) fn run(s: &mut Session, common: &Common) -> u8 {
    let Some(plan) = parse(&common.args) else { return 1 };
    let Some(mut cursors) = start(s, &plan) else { return 1 };
    let (mut seen, mut idle) = (0u64, Duration::from_millis(10));
    loop {
        let mut got = 0;
        for cursor in &mut cursors {
            let room = plan.limit.map_or(u64::MAX, |l| l.saturating_sub(seen + got));
            if room == 0 {
                break;
            }
            match read(s, &plan, cursor, room) {
                Batch::Frames(n) => got += n,
                Batch::Resync if plan.jump => {}
                Batch::Resync => return 3,
                Batch::Failed => return 2,
            }
        }
        seen += got;
        save(&plan, &cursors);
        if plan.limit.is_some_and(|l| seen >= l) {
            return 0;
        }
        idle = if got > 0 {
            Duration::from_millis(10)
        } else {
            (idle * 2).min(Duration::from_millis(500))
        };
        if got == 0 {
            std::thread::sleep(idle);
        }
    }
}

enum Batch {
    Frames(u64),
    Resync,
    Failed,
}

/// One FEED.READ on `cursor`'s shard, printing its frames.
fn read(s: &mut Session, plan: &Plan, cursor: &mut Cursor, room: u64) -> Batch {
    let (shard, generation, offset) =
        (cursor.shard.to_string(), cursor.generation.to_string(), cursor.offset.to_string());
    let mut argv: Vec<&[u8]> = vec![
        b"FEED.READ",
        shard.as_bytes(),
        generation.as_bytes(),
        offset.as_bytes(),
        b"COUNT",
        b"4096",
    ];
    if !plan.prefixes.is_empty() {
        argv.push(b"PREFIX");
        argv.extend(plan.prefixes.iter().map(Vec::as_slice));
    }
    let reply = match s.request(&argv) {
        Ok(r) => r,
        Err(e) => {
            eprint_bytes(&[b"Error: ", e.text().as_bytes(), b"\n"]);
            return Batch::Failed;
        }
    };
    match reply {
        Reply::Error(msg) if msg.starts_with(b"FEEDRESYNC") => resync(plan, cursor, &msg),
        Reply::Array(parts) => frames(plan, cursor, &parts, room),
        Reply::Error(msg) => {
            eprint_bytes(&[b"(error) ", &msg, b"\n"]);
            Batch::Failed
        }
        _ => Batch::Failed,
    }
}

/// Print up to `room` frames and move the cursor past the last printed one
/// (or to the batch's end when all were printed).
fn frames(plan: &Plan, cursor: &mut Cursor, parts: &[Reply], room: u64) -> Batch {
    let (Some(Reply::Int(generation)), Some(Reply::Int(next)), Some(Reply::Array(list))) =
        (parts.first(), parts.get(1), parts.get(2))
    else {
        return Batch::Failed;
    };
    let mut printed = 0u64;
    let mut resume = *next;
    for frame in list {
        let Reply::Array(pair) = frame else { continue };
        let (Some(Reply::Int(offset)), Some(Reply::Array(words))) = (pair.first(), pair.get(1))
        else {
            continue;
        };
        if printed == room {
            resume = *offset;
            break;
        }
        let argv: Vec<&[u8]> = words
            .iter()
            .filter_map(|w| if let Reply::Bulk(b) = w { Some(b.as_slice()) } else { None })
            .collect();
        write_out(&line(plan, cursor.shard, *generation, *offset, &argv));
        printed += 1;
    }
    (cursor.generation, cursor.offset) = (*generation, resume);
    Batch::Frames(printed)
}

/// A frame as a JSON line, or as the RESP command it carries.
fn line(plan: &Plan, shard: u64, generation: i64, offset: i64, argv: &[&[u8]]) -> Vec<u8> {
    if plan.resp {
        let mut out = Vec::new();
        kevy_resp::encode_command_borrowed(&mut out, argv);
        return out;
    }
    let words: Vec<String> = argv.iter().map(|w| super::render::json_string(w)).collect();
    format!(
        "{{\"shard\":{shard},\"generation\":{generation},\"offset\":{offset},\"argv\":[{}]}}\n",
        words.join(",")
    )
    .into_bytes()
}

/// `FEEDRESYNC <generation> <tail>`: say so, and move to the tail when told.
fn resync(plan: &Plan, cursor: &mut Cursor, msg: &[u8]) -> Batch {
    let words: Vec<&[u8]> = msg.split(|&b| b == b' ').collect();
    let number =
        |i: usize| words.get(i).and_then(|w| std::str::from_utf8(w).ok()?.parse::<i64>().ok());
    let (Some(generation), Some(tail)) = (number(1), number(2)) else { return Batch::Failed };
    let text = format!(
        "kevy-cli: shard {} needs a resync: its feed is now generation {generation}, tail {tail}; {}\n",
        cursor.shard,
        if plan.jump {
            "continuing from the tail"
        } else {
            "stopping (--on-resync jump continues from the tail)"
        }
    );
    eprint_bytes(&[text.as_bytes()]);
    (cursor.generation, cursor.offset) = (generation, tail);
    if plan.jump { Batch::Frames(0) } else { Batch::Resync }
}
