//! `SLOWLOG` — per-shard slow-command ring buffer and the GET/LEN/RESET/HELP
//! fan-out. Each shard owns its own [`SlowlogState`]; `SLOWLOG GET` and
//! `SLOWLOG LEN` aggregate across shards, `SLOWLOG RESET` clears them all.
//!
//! Timing position: the inline fast-path and `exec_op`'s [`Op::Dispatch`] arm
//! both measure `Instant::now()` around the dispatch call only (no AOF /
//! WATCH / notify overhead is charged to the recorded micros). Records only
//! when `state.slower_than_micros >= 0` AND elapsed micros strictly exceed
//! the threshold (Redis semantics). When OFF (`-1`), `Instant::now()` is
//! never called.

use std::collections::VecDeque;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::Commands;
use crate::message::{Agg, Op, Part};
use crate::shard::Shard;
use kevy_resp::{ArgvView, encode_array_len, encode_bulk, encode_integer};

/// One slow-command entry (Redis `SLOWLOG GET` field shape).
#[derive(Debug, Clone)]
pub struct SlowlogEntry {
    /// Globally unique id (`(shard_id << 48) | local_seq`). Monotonic
    /// per-shard; not globally monotonic (cross-shard SLOWLOG GET sorts
    /// by timestamp DESC anyway).
    pub id: u64,
    /// Unix epoch seconds at the time the command finished.
    pub timestamp_secs: i64,
    /// Wall-clock execution time in microseconds.
    pub micros: u64,
    /// The command argv (up to [`MAX_ARGV_RECORDED`] elements). Each
    /// element is owned bytes — the source `ArgvView` may not outlive
    /// the ring.
    pub argv: Vec<Vec<u8>>,
    /// "ip:port" of the client, or empty when unknown. v1 always empty;
    /// hooking sock.peer_addr() is left for a follow-up since the conn
    /// doesn't currently track its own addr.
    pub client_addr: Vec<u8>,
    /// `CLIENT SETNAME` value, or empty. v1 always empty.
    pub client_name: Vec<u8>,
}

/// Per-shard slowlog state — bundled into one field on [`Shard`] so the
/// 4-field add doesn't worsen `shard.rs`'s already-over-cap LOC count.
pub(crate) struct SlowlogState {
    pub(crate) buf: VecDeque<SlowlogEntry>,
    /// Record any command whose elapsed micros strictly exceed this
    /// value. `-1` disables (hot-path checks this first → zero clock
    /// reads); `0` records all (every commands' `Instant::now()`
    /// difference is > 0).
    pub(crate) slower_than_micros: i64,
    /// Maximum entries kept; oldest evicted on insert overflow.
    pub(crate) max_len: u32,
    /// Local sequence counter. Packed with `shard_id` into the public
    /// `id` so cross-shard merges retain uniqueness.
    pub(crate) next_local_seq: u64,
}

impl SlowlogState {
    pub(crate) fn new(slower_than_micros: i64, max_len: u32) -> Self {
        Self {
            buf: VecDeque::with_capacity(max_len.min(1024) as usize),
            slower_than_micros,
            max_len,
            next_local_seq: 0,
        }
    }
}

/// How many argv elements to keep in a recorded entry. Mirrors Redis's
/// `SLOWLOG_ENTRY_MAX_ARGC = 32` cap so a flood of huge MSET-like
/// commands doesn't bloat the ring.
const MAX_ARGV_RECORDED: usize = 32;

/// Cap on per-argument byte length recorded. Mirrors Redis's
/// `SLOWLOG_ENTRY_MAX_STRING = 128`.
const MAX_ARG_BYTES_RECORDED: usize = 128;

impl<C: Commands> Shard<C> {
    /// Record a slow-command entry if `elapsed_micros` exceeds the
    /// current threshold. Hot-path callers must early-out on the
    /// `slower_than_micros < 0` check BEFORE taking the `Instant::now()`
    /// pair; this function repeats the check defensively but does not
    /// remove the clock read from the caller.
    #[inline]
    pub(crate) fn slowlog_record<A: ArgvView + ?Sized>(
        &mut self,
        args: &A,
        elapsed_micros: u64,
    ) {
        let threshold = self.slowlog.slower_than_micros;
        if threshold < 0 {
            return;
        }
        // Skip strictly below threshold — `elapsed == threshold` records,
        // matching Redis's `if (duration < slowlog_log_slower_than) return;`
        // and making `slowlog-log-slower-than 0` record every command
        // (including the sub-microsecond `as_micros() → 0` ones that hit
        // in release-profile measurement).
        if (elapsed_micros as i64) < threshold {
            return;
        }
        let local_seq = self.slowlog.next_local_seq;
        self.slowlog.next_local_seq = self.slowlog.next_local_seq.wrapping_add(1);
        // Pack `(shard_id, local_seq)` so cross-shard ids stay unique.
        // 16 bits for shard_id is plenty (kevy targets ≤ 256 cores).
        let id = ((self.id as u64) << 48) | (local_seq & 0x0000_FFFF_FFFF_FFFF);
        let timestamp_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let mut argv: Vec<Vec<u8>> = Vec::with_capacity(args.len().min(MAX_ARGV_RECORDED));
        for i in 0..args.len().min(MAX_ARGV_RECORDED) {
            let a = &args[i];
            if a.len() > MAX_ARG_BYTES_RECORDED {
                argv.push(a[..MAX_ARG_BYTES_RECORDED].to_vec());
            } else {
                argv.push(a.to_vec());
            }
        }
        self.slowlog.buf.push_back(SlowlogEntry {
            id,
            timestamp_secs,
            micros: elapsed_micros,
            argv,
            client_addr: Vec::new(),
            client_name: Vec::new(),
        });
        let cap = self.slowlog.max_len as usize;
        while self.slowlog.buf.len() > cap {
            self.slowlog.buf.pop_front();
        }
    }

    /// Dispatch a `SLOWLOG GET/LEN/RESET/HELP` request. Help short-circuits
    /// to an immediate static reply; the other three fan out to every shard
    /// using the standard `Agg`/`Part` pipeline.
    pub(crate) fn start_slowlog(&mut self, conn_id: u64, seq: u64, sub: SlowlogSub) {
        match sub {
            SlowlogSub::Help => self.slowlog_immediate(conn_id, seq, slowlog_help_bytes()),
            SlowlogSub::Err(b) => self.slowlog_immediate(conn_id, seq, b),
            SlowlogSub::Reset => {
                self.slowlog_fanout(conn_id, seq, Agg::AllOk, || Op::SlowlogReset)
            }
            SlowlogSub::Len => {
                self.slowlog_fanout(conn_id, seq, Agg::SumInt(0), || Op::SlowlogLen)
            }
            SlowlogSub::Get(count) => self.slowlog_fanout(
                conn_id,
                seq,
                Agg::SlowlogGet { count, entries: Vec::new() },
                || Op::SlowlogGet,
            ),
        }
    }

    fn slowlog_immediate(&mut self, conn_id: u64, seq: u64, bytes: Vec<u8>) {
        self.push_pending_slot(conn_id, 1, Agg::First(None), false);
        self.fold(conn_id, seq, Part::Reply(bytes));
    }

    fn slowlog_fanout(
        &mut self,
        conn_id: u64,
        seq: u64,
        agg: Agg,
        mk_op: impl Fn() -> Op,
    ) {
        let targets: Vec<(usize, Op)> = (0..self.nshards).map(|s| (s, mk_op())).collect();
        self.push_pending_slot(conn_id, targets.len() as u32, agg, false);
        self.dispatch_targets(conn_id, seq, targets);
    }
}

/// Parsed `SLOWLOG <sub> [args]` decision — picked at routing time so
/// the runtime knows whether to fan out or short-circuit.
#[derive(Debug, Clone)]
pub enum SlowlogSub {
    /// `SLOWLOG GET [count]`. `None` = use Redis default of 10. `Some(n)`
    /// where `n < 0` means "all entries".
    Get(Option<i64>),
    /// `SLOWLOG LEN`.
    Len,
    /// `SLOWLOG RESET`.
    Reset,
    /// `SLOWLOG HELP`.
    Help,
    /// Routing-time error: malformed or unknown subcommand. The byte
    /// slice carries the full RESP error reply (e.g. `-ERR ...\r\n`)
    /// so dispatch is a one-step `Part::Reply`.
    Err(Vec<u8>),
}

/// RESP encoding of a completed [`Agg::SlowlogGet`]. Each entry is a
/// 6-element nested array per the Redis SLOWLOG GET wire spec:
/// `[id, ts_secs, micros, argv-array, client_addr, client_name]`.
/// Sorting is timestamp-DESC then id-DESC for ties; truncation to
/// `count` (or default 10) happens last.
pub(crate) fn encode_slowlog_get(count: Option<i64>, mut entries: Vec<SlowlogEntry>) -> Vec<u8> {
    entries.sort_by(|a, b| {
        b.timestamp_secs
            .cmp(&a.timestamp_secs)
            .then_with(|| b.id.cmp(&a.id))
    });
    let limit = match count {
        None => 10,
        Some(n) if n < 0 => entries.len(),
        Some(n) => n as usize,
    };
    let n = entries.len().min(limit);
    let mut out = Vec::with_capacity(64 + n * 64);
    encode_array_len(&mut out, n as i64);
    for e in entries.iter().take(n) {
        encode_array_len(&mut out, 6);
        encode_integer(&mut out, e.id as i64);
        encode_integer(&mut out, e.timestamp_secs);
        encode_integer(&mut out, e.micros as i64);
        encode_array_len(&mut out, e.argv.len() as i64);
        for a in &e.argv {
            encode_bulk(&mut out, a);
        }
        encode_bulk(&mut out, &e.client_addr);
        encode_bulk(&mut out, &e.client_name);
    }
    out
}

/// Static `SLOWLOG HELP` reply body (Redis text, lightly adapted).
pub(crate) fn slowlog_help_bytes() -> Vec<u8> {
    const LINES: &[&str] = &[
        "SLOWLOG <subcommand> [<arg> [value] [opt] ...]. Subcommands are:",
        "GET [<count>]",
        "    Return top <count> entries from the slowlog (default: 10, -1 mean all).",
        "    Entries are made of:",
        "    id, timestamp, time in microseconds, arguments array, client IP and port,",
        "    client name",
        "LEN",
        "    Return the length of the slowlog.",
        "RESET",
        "    Reset the slowlog.",
        "HELP",
        "    Print this help.",
    ];
    let mut out = Vec::with_capacity(512);
    encode_array_len(&mut out, LINES.len() as i64);
    for l in LINES {
        encode_bulk(&mut out, l.as_bytes());
    }
    out
}

/// Parse `args` ( `[verb, sub, ...]` ) into a [`SlowlogSub`]. Verb name
/// is assumed to already be SLOWLOG (the caller's route table dispatched
/// to here). Embedders call this from their `Commands::resolve` /
/// `Commands::route` impl.
pub fn parse_slowlog_sub<A: ArgvView + ?Sized>(args: &A) -> SlowlogSub {
    let Some(sub) = args.get(1) else {
        return SlowlogSub::Err(slowlog_err_bytes("wrong number of arguments for 'slowlog'"));
    };
    let mut buf = [0u8; 16];
    let upper = ascii_upper_into(sub, &mut buf);
    match upper {
        b"GET" => parse_slowlog_get(args),
        b"LEN" if args.len() == 2 => SlowlogSub::Len,
        b"RESET" if args.len() == 2 => SlowlogSub::Reset,
        b"HELP" => SlowlogSub::Help,
        b"LEN" | b"RESET" => SlowlogSub::Err(slowlog_arg_count_err(upper)),
        _ => SlowlogSub::Err(slowlog_unknown_sub_err(sub)),
    }
}

fn parse_slowlog_get<A: ArgvView + ?Sized>(args: &A) -> SlowlogSub {
    if args.len() == 2 {
        return SlowlogSub::Get(None);
    }
    if args.len() != 3 {
        return SlowlogSub::Err(slowlog_err_bytes(
            "wrong number of arguments for 'slowlog|get'",
        ));
    }
    match std::str::from_utf8(&args[2]).ok().and_then(|s| s.parse::<i64>().ok()) {
        Some(n) => SlowlogSub::Get(Some(n)),
        None => SlowlogSub::Err(slowlog_err_bytes("value is not an integer or out of range")),
    }
}

fn slowlog_arg_count_err(sub_upper: &[u8]) -> Vec<u8> {
    let lower: String = sub_upper.iter().map(|b| b.to_ascii_lowercase() as char).collect();
    slowlog_err_bytes(&format!("wrong number of arguments for 'slowlog|{lower}'"))
}

fn slowlog_unknown_sub_err(sub: &[u8]) -> Vec<u8> {
    let msg = format!(
        "ERR Unknown SLOWLOG subcommand or wrong number of arguments for '{}'",
        String::from_utf8_lossy(sub),
    );
    let mut out = Vec::with_capacity(msg.len() + 3);
    out.push(b'-');
    out.extend_from_slice(msg.as_bytes());
    out.extend_from_slice(b"\r\n");
    out
}

fn slowlog_err_bytes(msg: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(msg.len() + 7);
    out.extend_from_slice(b"-ERR ");
    out.extend_from_slice(msg.as_bytes());
    out.extend_from_slice(b"\r\n");
    out
}

fn ascii_upper_into<'a>(src: &[u8], buf: &'a mut [u8; 16]) -> &'a [u8] {
    let n = src.len().min(buf.len());
    for i in 0..n {
        buf[i] = src[i].to_ascii_uppercase();
    }
    &buf[..n]
}
