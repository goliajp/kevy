//! FEED.* consumer surface.
//!
//! - `FEED.SHARDS` → `:nshards` (answered locally — every shard knows).
//! - `FEED.TAIL <shard>` → `*2 [:generation, :next_offset]`, executed
//!   on the target shard.
//! - `FEED.READ <shard> <gen> <offset> [COUNT n] [PREFIX p ...]` →
//!   `*3 [:generation, :next_cursor_offset, *N frames]`, each frame
//!   `*2 [:offset, *M argv]`. Unservable cursors answer
//!   `-FEEDRESYNC <gen> <tail>`; a cursor ahead of the stream answers
//!   `-ERR feed cursor ahead of stream`.
//!
//! Prefix filtering is fail-open: a frame whose key layout the filter
//! can't cheaply determine (multi-key DEL/MSET, FLUSHALL, …) is always
//! delivered — over-delivery is free under at-least-once, silent drops
//! are not. Filtering never moves the cursor differently: `next` is
//! the offset after the last *scanned* frame, matched or not.

use kevy_resp::CmdError;
use kevy_replicate::feed::FeedRead;
use kevy_resp::ArgvView;

use kevy_resp::Argv;

use crate::message::{Agg, Op, Part, SmallReply};
use crate::shard::Shard;
use crate::Commands;

/// Cap on frames one FEED.READ scans (COUNT is clamped to this).
const FEED_READ_MAX: usize = 4096;

/// Parsed FEED.READ arguments.
pub(crate) struct FeedReadArgs {
    pub shard: usize,
    pub cursor_gen: u64,
    pub offset: u64,
    pub count: usize,
    pub prefixes: Vec<Vec<u8>>,
}

/// Parse `FEED.TAIL <shard>`'s shard index.
pub(crate) fn parse_shard_arg<A: ArgvView + ?Sized>(args: &A) -> Result<usize, CmdError> {
    if args.len() != 2 {
        return Err(CmdError::Wire("ERR wrong number of arguments for 'feed.tail'"));
    }
    std::str::from_utf8(&args[1])
        .ok()
        .and_then(|s| s.parse().ok())
        .ok_or(CmdError::Wire("ERR invalid shard"))
}

/// Parse `FEED.READ <shard> <gen> <offset> [COUNT n] [PREFIX p ...]`.
pub(crate) fn parse_feed_read<A: ArgvView + ?Sized>(args: &A) -> Result<FeedReadArgs, CmdError> {
    if args.len() < 4 {
        return Err(CmdError::Wire("ERR wrong number of arguments for 'feed.read'"));
    }
    let int = |i: usize| -> Option<u64> {
        std::str::from_utf8(&args[i]).ok().and_then(|s| s.parse().ok())
    };
    let shard = int(1).ok_or("ERR invalid shard")? as usize;
    let cursor_gen = int(2).ok_or("ERR invalid generation")?;
    let offset = int(3).ok_or("ERR invalid offset")?;
    let mut count = 256usize;
    let mut prefixes = Vec::new();
    let mut i = 4;
    while i < args.len() {
        let a = &args[i];
        if a.eq_ignore_ascii_case(b"COUNT") {
            count = int(i + 1).ok_or("ERR invalid COUNT")? as usize;
            i += 2;
        } else if a.eq_ignore_ascii_case(b"PREFIX") {
            let p = args.get(i + 1).ok_or("ERR PREFIX requires a value")?;
            prefixes.push(p.to_vec());
            i += 2;
        } else {
            return Err(CmdError::Wire("ERR syntax error"));
        }
    }
    Ok(FeedReadArgs {
        shard,
        cursor_gen,
        offset,
        count: count.clamp(1, FEED_READ_MAX),
        prefixes,
    })
}

impl<C: Commands> Shard<C> {
    /// Entry from `start_command`: parse the FEED.* argv shape and
    /// dispatch to the target shard (or answer locally / with a parse
    /// error).
    pub(crate) fn start_feed_route<A: ArgvView + ?Sized>(
        &mut self,
        conn_id: u64,
        seq: u64,
        args: &A,
        route: &crate::Route,
        is_quit: bool,
    ) {
        match route {
            crate::Route::FeedShards => {
                self.push_pending_slot(conn_id, 1, Agg::First(None), is_quit);
                let n = self.nshards;
                self.fold(
                    conn_id,
                    seq,
                    Part::Reply(SmallReply::from_vec(format!(":{n}\r\n").into_bytes())),
                );
            }
            crate::Route::FeedTail => match parse_shard_arg(args) {
                Ok(sh) => self.start_feed_op(conn_id, seq, sh, Op::FeedTail, is_quit),
                Err(msg) => self.reply_feed_error(conn_id, seq, msg.as_wire(), is_quit),
            },
            crate::Route::FeedRead => match parse_feed_read(args) {
                Ok(p) => {
                    let op = Op::FeedRead {
                        cursor_gen: p.cursor_gen,
                        offset: p.offset,
                        count: p.count,
                        prefixes: p.prefixes,
                    };
                    self.start_feed_op(conn_id, seq, p.shard, op, is_quit);
                }
                Err(msg) => self.reply_feed_error(conn_id, seq, msg.as_wire(), is_quit),
            },
            _ => {}
        }
    }

    /// Ship a feed op to its target shard (or exec locally), replying
    /// through a plain `Agg::First` slot.
    pub(crate) fn start_feed_op(&mut self, conn_id: u64, seq: u64, shard: usize, op: Op, is_quit: bool) {
        self.push_pending_slot(conn_id, 1, Agg::First(None), is_quit);
        if shard >= self.nshards {
            self.fold(
                conn_id,
                seq,
                Part::Reply(SmallReply::from_vec(b"-ERR shard out of range\r\n".to_vec())),
            );
            return;
        }
        if shard == self.id {
            let part = self.exec_op(op);
            self.fold(conn_id, seq, part);
        } else {
            self.send_to(
                shard,
                crate::message::Inbound::Request { origin: self.id, conn: conn_id, seq, op },
            );
        }
    }

    /// Complete a feed slot with a parse error.
    pub(crate) fn reply_feed_error(&mut self, conn_id: u64, seq: u64, msg: &str, is_quit: bool) {
        self.push_pending_slot(conn_id, 1, Agg::First(None), is_quit);
        self.fold(
            conn_id,
            seq,
            Part::Reply(SmallReply::from_vec(format!("-{msg}\r\n").into_bytes())),
        );
    }

    /// `Op::FeedTail` — executed on the owning shard.
    pub(crate) fn exec_feed_tail(&mut self) -> Part {
        let Some(f) = &self.replicate else {
            return Part::Reply(SmallReply::from_vec(feed_disabled()));
        };
        let (generation, next) = f.tail();
        let mut out = Vec::with_capacity(32);
        out.extend_from_slice(b"*2\r\n");
        out.extend_from_slice(format!(":{generation}\r\n:{next}\r\n").as_bytes());
        Part::Reply(SmallReply::from_vec(out))
    }

    /// `Op::FeedRead` — executed on the owning shard.
    pub(crate) fn exec_feed_read(
        &mut self,
        cursor_gen: u64,
        offset: u64,
        count: usize,
        prefixes: Vec<Vec<u8>>,
    ) -> Part {
        let Some(f) = &self.replicate else {
            return Part::Reply(SmallReply::from_vec(feed_disabled()));
        };
        let frames = match f.read(cursor_gen, offset, count) {
            Ok(v) => v,
            Err(FeedRead::Resync { generation, tail }) => {
                return Part::Reply(SmallReply::from_vec(
                    format!("-FEEDRESYNC {generation} {tail}\r\n").into_bytes(),
                ));
            }
            Err(FeedRead::Future) => {
                return Part::Reply(SmallReply::from_vec(
                    b"-ERR feed cursor ahead of stream\r\n".to_vec(),
                ));
            }
        };
        let generation = f.generation();
        // The next cursor = one past the last SCANNED frame (filter
        // does not move it differently), or the request offset when
        // the read was empty (caught up).
        let next = frames.last().map_or(offset, |fr| fr.offset + 1);
        let mut body = Vec::new();
        let mut kept = 0usize;
        for fr in &frames {
            let Ok((foff, argv, _)) = kevy_replicate::wire::decode_frame(fr.bytes) else {
                continue; // torn frame cannot occur in-backlog; skip defensively
            };
            if !prefixes.is_empty() && !frame_matches(&self.commands, &argv, &prefixes) {
                continue;
            }
            kept += 1;
            body.extend_from_slice(format!("*2\r\n:{foff}\r\n*{}\r\n", argv.len()).as_bytes());
            for i in 0..argv.len() {
                let item = &argv[i];
                body.extend_from_slice(format!("${}\r\n", item.len()).as_bytes());
                body.extend_from_slice(item);
                body.extend_from_slice(b"\r\n");
            }
        }
        feed_read_reply(generation, next, kept, &body)
    }
}

/// Assemble the `FEED.READ` reply envelope (`*3 :gen :next *kept …`).
/// Extracted verbatim from [`Shard::exec_feed_read`] (single call site,
/// `inline(always)`) purely for the 50-LOC fn rule.
#[inline(always)]
fn feed_read_reply(generation: u64, next: u64, kept: usize, body: &[u8]) -> Part {
    let mut out = Vec::with_capacity(body.len() + 48);
    out.extend_from_slice(format!("*3\r\n:{generation}\r\n:{next}\r\n*{kept}\r\n").as_bytes());
    out.extend_from_slice(body);
    Part::Reply(SmallReply::from_vec(out))
}

fn feed_disabled() -> Vec<u8> {
    b"-ERR feed disabled (start with feed_enabled or replication on)\r\n".to_vec()
}

/// Prefix match, fail-open: only single-key routes are filtered (the
/// route tells us where the key sits); everything else is delivered.
fn frame_matches<C: Commands>(commands: &C, argv: &Argv, prefixes: &[Vec<u8>]) -> bool {
    match commands.route(argv) {
        crate::Route::Single(idx) => match argv.get(idx) {
            Some(key) => prefixes.iter().any(|p| key.starts_with(p.as_slice())),
            None => true,
        },
        _ => true,
    }
}
