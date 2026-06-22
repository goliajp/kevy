//! **v1.25 B.4 + A.2** — BigBulk SET ingest path for the io_uring reactor.
//!
//! The Phase A decompositions
//! ([`.claude/notes/v125-deco-axis-i-c50-10kb.md`] +
//! [`.claude/notes/v125-deco-axis-b-64kb.md`]) identified two amplifiers
//! on big SET writes:
//!
//! - **64 KiB SET** (Axis B): the multishot recv path splits the body
//!   into 5 × 16 KiB chunks; each chunk gets memcpy'd from the kernel
//!   slab into `conn.input` (with realloc storm 0→16→32→48→64K on a
//!   cold conn), then the entire 64 KiB gets memcpy'd AGAIN inside
//!   `Arc::from(&[u8])` when SET adopts the value into a `Value::ArcBulk`.
//!   Total: 128 KiB of memcpy per 64 KiB SET, vs valkey's BIG_ARG path
//!   that hot-swaps `c->querybuf` into the robj with zero post-recv copy.
//!
//! - **10 KiB SET** (Axis I): the G2 fast slab path already eliminated
//!   the slab→input memcpy for single-CQE frames, but the
//!   `Arc::from(&[u8])` still memcpys 10 KiB once per SET.
//!
//! This module installs a per-conn state machine that, on detecting the
//! `SET key $<N>\r\n` header with N ≥ [`BIG_ARG_PROMOTE_THRESHOLD`],
//! pre-allocates a `Vec<u8>` of size `N + 2`, routes subsequent multishot
//! recv CQE bytes into THAT Vec instead of `conn.input`, and on
//! completion adopts the Vec's heap allocation into the `Arc<[u8]>`
//! zero-copy via `Vec → into_boxed_slice → Arc::from(Box<[u8]>)` — see
//! [`kevy_store::Store::set`] which routes to
//! [`kevy_store::string::pick_value_for_set_owned`] for the adoption.
//!
//! Scope: SET only (the standard 3-arg `SET key value` form). MSET /
//! SETEX / APPEND / GETSET / HSET-big-value etc. keep the borrowed-slice
//! path; extending to them is a follow-up once the SET-only state
//! machine is proven on the bench.
//!
//! The orthodox alternative — adding a `take()` method to `kevy_resp`'s
//! `ArgvView` trait so the dispatch layer can steal ownership from a
//! parse buffer — was investigated and rejected (see agent a2d255b6's
//! verification report + `bench/V125-OPEN-ITEMS.md` 2026-06-22 update):
//! `Vec::split_off` is NOT zero-copy in stdlib, the G2 fast slab path
//! gets a borrow into a kernel-shared slab where take is undefined, and
//! `Argv` packs all args into one packed `Vec<u8>`. The per-conn state
//! machine here sidesteps all three by recv'ing the SET value's body
//! into its OWN owned Vec from the start.

use crate::Commands;
use crate::shard::Shard;
use crate::uring_conn::{BigArgSetOptions, BigArgState, UringConn};
use kevy_map::KevyMap;
use kevy_resp::{encode_error, encode_null_bulk, encode_simple_string};

/// **v1.25 B.4 + A.2** — threshold (in body bytes) at which a `SET key
/// <value>` command promotes into the BigBulk recv-into-owned-Vec path
/// instead of the borrowed-slice `set_slice` path.
///
/// Picked at 4 KiB:
/// - Small enough that the existing `Arc::from(&[u8])` memcpy (~0.6 µs at
///   4 KiB → ~10 µs at 64 KiB) is worth the BigBulk state-machine cost.
/// - Aligned with valkey's `PROTO_MBULK_BIG_ARG = 32 KiB` order of
///   magnitude (we go lower because our G2 fast slab path already wins
///   the single-CQE case; we need to also cover multi-CQE values that
///   the slow path otherwise reallocs into `conn.input`).
/// - For Axis I (10 KiB SET) this fires; for Axis B (64 KiB SET) this
///   fires; for typical Redis workloads of 64-byte to 4-KiB values it
///   stays on the borrowed-slice path (no change).
pub(crate) const BIG_ARG_PROMOTE_THRESHOLD: usize = 4 * 1024;

/// Maximum body length we'll accept into a BigBulk dest Vec — matches
/// Redis' default `proto-max-bulk-len` so a malicious client can't OOM
/// the server by sending a giant `$<N>\r\n`.
const MAX_BULK_LEN: usize = 512 * 1024 * 1024;

/// Outcome of a `SET key …` BigBulk-promotion probe against the head
/// of a freshly-arrived buffer.
pub(crate) enum BigArgProbe {
    /// Buffer head doesn't match the `SET key $<N>\r\n …` pattern, OR the
    /// value bulk is smaller than the promotion threshold, OR the header
    /// itself isn't fully present yet. Caller proceeds with the normal
    /// borrowed-slice dispatch.
    NotApplicable,
    /// Frame head is `SET key $<N>\r\n` with `N ≥ BIG_ARG_PROMOTE_THRESHOLD`;
    /// `header_end` is the byte offset where the value body starts in
    /// `buf` (i.e. just past the value-bulk `\r\n`). Caller promotes:
    /// pre-allocate `Vec::with_capacity(body_len + 2)`, copy any
    /// already-received body bytes (slab tail past `header_end`), and
    /// install [`BigArgState`] on the conn.
    Promote {
        header_end: usize,
        body_len: usize,
        key: Box<[u8]>,
        opts: BigArgSetOptions,
    },
}

/// Walk an ASCII decimal integer at `buf[i..]` until a non-digit; returns
/// the parsed value and the index just past the last digit. Caps at
/// `usize::MAX / 10` to avoid overflow before reaching a final length.
fn parse_decimal_at(buf: &[u8], mut i: usize) -> Option<(usize, usize)> {
    let start = i;
    let mut n: usize = 0;
    while i < buf.len() && buf[i].is_ascii_digit() {
        if n > usize::MAX / 10 {
            return None;
        }
        n = n * 10 + (buf[i] - b'0') as usize;
        i += 1;
    }
    if i == start {
        return None;
    }
    Some((n, i))
}

/// Probe whether `buf` starts with a `*3\r\n$3\r\nSET\r\n$<keylen>\r\n
/// <key>\r\n$<N>\r\n` shape where `N >= BIG_ARG_PROMOTE_THRESHOLD` AND
/// every byte through the value-bulk **header `\r\n`** is present. The
/// value **body** does NOT have to be present — that's the whole point
/// (we promote to BigBulk *before* the body arrives so subsequent CQEs
/// land in the owned dest Vec rather than the conn's `input` Vec).
///
/// Returns [`BigArgProbe::Promote`] on success with `header_end =` byte
/// offset of the first body byte; `BigArgProbe::NotApplicable` otherwise.
/// Bails fast on the non-SET, small-value, or pipelined-non-SET-first
/// shapes — must stay under ~30 ns of work per call.
///
/// Scope: 3-arg form (`*3`) only. SET with options (`SET k v EX 10`) is
/// rejected by the `argc != 3` gate so option parsing isn't required
/// here — the value body would have to be present to parse options that
/// follow it. The 3-arg form is what redis-benchmark and dogfood
/// projects send.
/// Verify the `*3\r\n$3\r\n<verb>\r\n` prefix where `<verb>` is a
/// case-insensitive `SET`. Returns the byte offset just past the verb
/// CRLF on match, or `None` otherwise.
fn probe_set_verb_prefix(buf: &[u8]) -> Option<usize> {
    if buf.first() != Some(&b'*') {
        return None;
    }
    let (argc, after_argc_digits) = parse_decimal_at(buf, 1)?;
    if argc != 3 {
        // SET with options (`SET k v EX 10`) — see scope note above.
        return None;
    }
    if buf.get(after_argc_digits..after_argc_digits + 2) != Some(b"\r\n") {
        return None;
    }
    let p = after_argc_digits + 2;
    let verb_block = buf.get(p..p + 9)?;
    if &verb_block[..4] != b"$3\r\n" || &verb_block[7..] != b"\r\n" {
        return None;
    }
    let a = verb_block[4].to_ascii_uppercase();
    let b = verb_block[5].to_ascii_uppercase();
    let c = verb_block[6].to_ascii_uppercase();
    if [a, b, c] != *b"SET" {
        return None;
    }
    Some(p + 9)
}

/// Parse `$<keylen>\r\n<key>\r\n` at `buf[start..]`. Returns the owned
/// key bytes and the byte offset just past the key's CRLF, or `None`
/// if the key bulk isn't fully present yet.
fn probe_key_bulk(buf: &[u8], start: usize) -> Option<(Box<[u8]>, usize)> {
    if buf.get(start) != Some(&b'$') {
        return None;
    }
    let (keylen, after_keylen) = parse_decimal_at(buf, start + 1)?;
    if buf.get(after_keylen..after_keylen + 2) != Some(b"\r\n") {
        return None;
    }
    let key_start = after_keylen + 2;
    if buf.len() < key_start + keylen + 2 {
        return None;
    }
    if buf.get(key_start + keylen..key_start + keylen + 2) != Some(b"\r\n") {
        return None;
    }
    let key_end = key_start + keylen;
    let key_bytes = buf[key_start..key_end].to_vec().into_boxed_slice();
    Some((key_bytes, key_end + 2))
}

pub(crate) fn probe_set_bigbulk(buf: &[u8]) -> BigArgProbe {
    let Some(after_verb) = probe_set_verb_prefix(buf) else {
        return BigArgProbe::NotApplicable;
    };
    let Some((key, after_key)) = probe_key_bulk(buf, after_verb) else {
        return BigArgProbe::NotApplicable;
    };
    // $<N>\r\n — body bulk header only; body bytes don't have to be present.
    if buf.get(after_key) != Some(&b'$') {
        return BigArgProbe::NotApplicable;
    }
    let Some((body_len, after_body_len_digits)) = parse_decimal_at(buf, after_key + 1) else {
        return BigArgProbe::NotApplicable;
    };
    if body_len < BIG_ARG_PROMOTE_THRESHOLD {
        return BigArgProbe::NotApplicable;
    }
    if buf.get(after_body_len_digits..after_body_len_digits + 2) != Some(b"\r\n") {
        return BigArgProbe::NotApplicable;
    }
    BigArgProbe::Promote {
        header_end: after_body_len_digits + 2,
        body_len,
        key,
        opts: BigArgSetOptions::default(),
    }
}

impl<C: Commands> Shard<C> {
    /// **v1.25 B.4 + A.2** — try to promote the conn into BigBulk-recv
    /// mode based on `tail`'s contents. Returns `true` iff the head of
    /// `tail` matched `SET key $<N>` with `N ≥ BIG_ARG_PROMOTE_THRESHOLD`
    /// (no options, RESP `*3` form) AND state was successfully installed.
    /// On match: any body bytes already in `tail` past the value-bulk
    /// header land in the dest Vec; subsequent multishot CQEs feed
    /// directly into the same Vec via [`Self::uring_bigbulk_feed`].
    ///
    /// When the body completes in the SAME slab that carried the
    /// header (pathological — only possible on the slow path where
    /// prior buffered bytes pushed the parse point past the header),
    /// the SET is finalised inline and no state is installed.
    pub(crate) fn try_promote_bigbulk(
        &mut self,
        cid: u64,
        tail: &[u8],
        io: &mut KevyMap<u64, UringConn>,
    ) -> bool {
        let BigArgProbe::Promote { header_end, body_len, key, opts } =
            probe_set_bigbulk(tail)
        else {
            return false;
        };
        let Some(uc) = io.get_mut(&cid) else { return false };
        if uc.pending_big_arg.is_some() {
            // Shouldn't happen — body completion clears it before we
            // process more bytes. Defensive: bail to the normal path.
            return false;
        }
        if body_len > MAX_BULK_LEN {
            return false;
        }
        // Capacity = body_len EXACTLY so `Vec::into_boxed_slice` later
        // adopts the heap allocation without a realloc-shrink — that's
        // the whole zero-copy point of this path.
        let mut buf = Vec::with_capacity(body_len);
        let mut crlf_needed: u8 = 2;
        // Body bytes that arrived in the same chunk as the header.
        let body_bytes_present = tail.len().saturating_sub(header_end);
        let body_take = body_bytes_present.min(body_len);
        if body_take > 0 {
            buf.extend_from_slice(&tail[header_end..header_end + body_take]);
        }
        // CRLF bytes that arrived right after the body in the same chunk.
        let after_body = header_end + body_take;
        let crlf_take = (tail.len().saturating_sub(after_body))
            .min(crlf_needed as usize);
        crlf_needed -= crlf_take as u8;
        let state = BigArgState { buf, body_len, crlf_needed, key, opts };
        let complete_now = state.buf.len() == body_len && state.crlf_needed == 0;
        if complete_now {
            // Pathological-but-possible: header AND full body+CRLF landed
            // in the same chunk and the chunk wasn't dispatched via the
            // fast path. Apply inline; don't install BigBulk state.
            let BigArgState { buf, key, opts, .. } = state;
            self.uring_apply_bigbulk_set(cid, key, buf, opts);
            return true;
        }
        uc.pending_big_arg = Some(Box::new(state));
        true
    }

    /// **v1.25 B.4 + A.2** — append slab bytes into the conn's
    /// `pending_big_arg.buf`, completing the SET when `buf.len() == body_len
    /// + 2`. Excess bytes past the frame end are a pipelined next command;
    /// they get routed through the regular dispatch path so the next
    /// frame in the pipeline can also promote (or just execute normally).
    pub(crate) fn uring_bigbulk_feed(
        &mut self,
        cid: u64,
        io: &mut KevyMap<u64, UringConn>,
        slab: &[u8],
    ) {
        let Some(uc) = io.get_mut(&cid) else { return };
        let Some(state) = uc.pending_big_arg.as_mut() else { return };
        // Phase 1: fill the body. `buf` has capacity EXACTLY `body_len`
        // so this `extend_from_slice` never reallocates — the kernel
        // bytes land directly in the Vec that becomes the Arc body
        // (zero-copy adoption at completion).
        let body_remaining = state.body_len - state.buf.len();
        let body_take = slab.len().min(body_remaining);
        if body_take > 0 {
            state.buf.extend_from_slice(&slab[..body_take]);
        }
        // Phase 2: drain the trailing CRLF off the wire. Protocol
        // framing — not appended to `buf`, just counted off.
        let mut p = body_take;
        if state.crlf_needed > 0 && p < slab.len() {
            let crlf_take = (slab.len() - p).min(state.crlf_needed as usize);
            state.crlf_needed -= crlf_take as u8;
            p += crlf_take;
        }
        if state.buf.len() == state.body_len && state.crlf_needed == 0 {
            let state = uc.pending_big_arg.take().expect("just observed");
            let BigArgState { buf, key, opts, .. } = *state;
            self.uring_apply_bigbulk_set(cid, key, buf, opts);
        }
        if p < slab.len() {
            self.uring_bigbulk_feed_pipelined(cid, io, &slab[p..]);
        }
    }

    /// Route pipelined bytes past the BigBulk frame through the regular
    /// dispatch path — they might be a fresh SET that itself promotes
    /// (the recursion bottoms out naturally), or a small command, or a
    /// partial frame that gets staged into `conn.input`.
    fn uring_bigbulk_feed_pipelined(
        &mut self,
        cid: u64,
        io: &mut KevyMap<u64, UringConn>,
        extra: &[u8],
    ) {
        let mut input_buf = match self.conns.get_mut(&cid) {
            Some(c) => std::mem::take(&mut c.input),
            None => return,
        };
        let outcome = self.uring_recv_dispatch(cid, extra, &mut input_buf, io);
        if outcome.conn_gone {
            return;
        }
        if let Some(c) = self.conns.get_mut(&cid) {
            c.input = input_buf;
        }
        if outcome.protocol_error {
            self.protocol_error(cid);
            self.uring_mark_closing(cid, io);
        }
    }

    /// **v1.25 B.4 + A.2** — finalise a BigBulk SET: build the Value
    /// zero-copy from the owned body Vec, call `Store::set`, emit the
    /// reply (`+OK` / `$-1`). Mirrors the SET arm of
    /// [`kevy::cmd_data::cmd_set`].
    fn uring_apply_bigbulk_set(
        &mut self,
        cid: u64,
        key: Box<[u8]>,
        value: Vec<u8>,
        opts: BigArgSetOptions,
    ) {
        let Some(conn) = self.conns.get_mut(&cid) else { return };
        if opts.syntax_error {
            encode_error(&mut conn.output, "ERR syntax error");
            conn.next_emit += 1;
            return;
        }
        let ok = self.store.set(&key, value, opts.expire, opts.nx, opts.xx);
        let Some(conn) = self.conns.get_mut(&cid) else { return };
        if ok {
            encode_simple_string(&mut conn.output, "OK");
        } else {
            encode_null_bulk(&mut conn.output);
        }
        conn.next_emit += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_set_frame(key: &[u8], val_len: usize) -> Vec<u8> {
        let mut f = Vec::new();
        f.extend_from_slice(b"*3\r\n$3\r\nSET\r\n$");
        f.extend_from_slice(key.len().to_string().as_bytes());
        f.extend_from_slice(b"\r\n");
        f.extend_from_slice(key);
        f.extend_from_slice(b"\r\n$");
        f.extend_from_slice(val_len.to_string().as_bytes());
        f.extend_from_slice(b"\r\n");
        // Body bytes intentionally NOT included — probe runs at header end.
        f
    }

    #[test]
    fn probe_matches_4k_set_just_header() {
        let frame = make_set_frame(b"key", 4096);
        match probe_set_bigbulk(&frame) {
            BigArgProbe::Promote { header_end, body_len, key, .. } => {
                assert_eq!(header_end, frame.len());
                assert_eq!(body_len, 4096);
                assert_eq!(&*key, b"key");
            }
            _ => panic!("expected Promote"),
        }
    }

    #[test]
    fn probe_matches_64k_set() {
        let frame = make_set_frame(b"k", 65536);
        assert!(matches!(probe_set_bigbulk(&frame), BigArgProbe::Promote { body_len: 65536, .. }));
    }

    #[test]
    fn probe_rejects_small_set_below_threshold() {
        let frame = make_set_frame(b"k", 100);
        assert!(matches!(probe_set_bigbulk(&frame), BigArgProbe::NotApplicable));
    }

    #[test]
    fn probe_rejects_threshold_minus_one() {
        let frame = make_set_frame(b"k", BIG_ARG_PROMOTE_THRESHOLD - 1);
        assert!(matches!(probe_set_bigbulk(&frame), BigArgProbe::NotApplicable));
    }

    #[test]
    fn probe_matches_threshold_exact() {
        let frame = make_set_frame(b"k", BIG_ARG_PROMOTE_THRESHOLD);
        assert!(matches!(probe_set_bigbulk(&frame), BigArgProbe::Promote { .. }));
    }

    #[test]
    fn probe_rejects_set_with_options() {
        // SET k v EX 10 → *5
        let mut frame = Vec::new();
        frame.extend_from_slice(b"*5\r\n$3\r\nSET\r\n$1\r\nk\r\n$");
        frame.extend_from_slice(b"4096\r\n");
        // Options ignored intentionally — probe should bail at argc!=3.
        assert!(matches!(probe_set_bigbulk(&frame), BigArgProbe::NotApplicable));
    }

    #[test]
    fn probe_rejects_get_command() {
        let frame = b"*2\r\n$3\r\nGET\r\n$1\r\nk\r\n";
        assert!(matches!(probe_set_bigbulk(frame), BigArgProbe::NotApplicable));
    }

    #[test]
    fn probe_rejects_truncated_header() {
        let full = make_set_frame(b"k", 16384);
        // Cut the header at every byte before complete — all must bail.
        for cut in 0..full.len() {
            assert!(matches!(
                probe_set_bigbulk(&full[..cut]),
                BigArgProbe::NotApplicable
            ));
        }
    }

    #[test]
    fn probe_accepts_lowercase_set() {
        let mut f = Vec::new();
        f.extend_from_slice(b"*3\r\n$3\r\nset\r\n$1\r\nk\r\n$4096\r\n");
        assert!(matches!(probe_set_bigbulk(&f), BigArgProbe::Promote { .. }));
    }

    #[test]
    fn probe_header_end_marks_first_body_byte() {
        // Header followed by a partial body — header_end should land
        // exactly on the first body byte.
        let mut f = make_set_frame(b"hello", 8192);
        let header_len = f.len();
        f.extend_from_slice(&[b'X'; 100]);
        match probe_set_bigbulk(&f) {
            BigArgProbe::Promote { header_end, .. } => {
                assert_eq!(header_end, header_len);
                assert_eq!(f[header_end], b'X');
            }
            _ => panic!("expected Promote"),
        }
    }
}
