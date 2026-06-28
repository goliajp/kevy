//! **v1.25 B.5 (post-2026-06-22)** — RESP frame probes for the BigBulk
//! ingest path. Split out of [`crate::uring_bigbulk`] so each file stays
//! under the 500-LOC house rule.
//!
//! Two probes live here:
//!
//! - [`probe_set_bigbulk`] — specialised, recognises only the `*3 $3 SET
//!   $klen key $N` shape (plain 3-arg SET). Used by the SET-bare fast
//!   path which **adopts the body Vec directly into the value Arc**
//!   (zero-copy: the Vec's heap allocation becomes the `Arc<[u8]>` body
//!   via `Vec::into_boxed_slice() → Arc::from(Box<[u8]>)`). Kept as a
//!   dedicated probe to preserve the B.4 win for the bare-SET hot path.
//!
//! - [`probe_generic_bigbulk`] — generic, recognises ANY RESP `*<argc>`
//!   frame whose **last** bulk has `N ≥ BIG_ARG_PROMOTE_THRESHOLD` and
//!   whose body for that last bulk isn't fully present in the slab head.
//!   Catches `SETEX key ttl <BIG>`, `PSETEX key ms <BIG>`, `APPEND key
//!   <BIG>`, `GETSET key <BIG>`, and `MSET k1 v1 … kn <BIG>`. Promotes
//!   into a frame-stitch mode that buffers the FULL RESP frame and
//!   re-dispatches it through the normal parser at completion — no
//!   typed-enum option re-parsing required.
//!
//! Restriction (v1.25 B.5 scope): the big bulk must be the **last** bulk
//! in the frame. This excludes `SET k <BIG> EX 10` (big value is bulk #3
//! of 5; trailing `EX 10` is two more small bulks) and is a v1.25.x
//! follow-up — the existing borrowed-slice path keeps working for that
//! shape, just without zero-copy ingest.

/// **v1.25 B.4 + A.2** — threshold (in body bytes) at which a `SET key
/// <value>` (or any other big-value command) command promotes into the
/// BigBulk recv path instead of the borrowed-slice path.
///
/// Picked at 4 KiB — see [`crate::uring_bigbulk`] module docs for the
/// reasoning.
pub(crate) const BIG_ARG_PROMOTE_THRESHOLD: usize = 4 * 1024;

/// Maximum body length we'll accept into a BigBulk dest Vec — matches
/// Redis' default `proto-max-bulk-len` so a malicious client can't OOM
/// the server by sending a giant `$<N>\r\n`.
pub(crate) const MAX_BULK_LEN: usize = 512 * 1024 * 1024;

/// **v1.25 B.5** — defensive cap on the number of bulks the generic
/// probe will walk before bailing. RESP allows up to `*<argc>` = INT_MAX,
/// but realistic big-value commands have ≤ 4 bulks (SETEX, APPEND, …);
/// MSET could be wide but we don't promote MSET unless the LAST value is
/// big, in which case we still walk every bulk to compute the total
/// frame length. Cap at 1024 to bound worst-case CPU on a hostile
/// `*1000000` header.
const MAX_PROBE_BULKS: usize = 1024;

/// Outcome of the generic last-bulk-big probe. Used for SETEX / APPEND /
/// GETSET / PSETEX / MSET-with-big-last-value.
pub(crate) enum BigArgGenericProbe {
    NotApplicable,
    /// Frame is `*<argc>` with the LAST bulk having `N ≥
    /// BIG_ARG_PROMOTE_THRESHOLD` and not fully present in the slab head.
    /// `total` is the entire RESP frame length (header + every bulk's
    /// `$<N>\r\n + N bytes + \r\n`); `bytes_present` is how many of those
    /// `total` bytes are already in the slab head (always ≤ `total`).
    /// Caller pre-allocates `Vec::with_capacity(total)`, copies the head,
    /// installs the BigBulk frame-stitch state, and re-dispatches the
    /// assembled bytes on completion.
    ///
    /// v1.29 (B3) — added `body_start_in_tail`, `body_len`, and
    /// `bare_set_key_range` so the caller can split a per-conn dest
    /// state into a header buf + a sized body buf, and (for the bare
    /// `SET key <BIG>` shape) bypass the dispatch_batch re-parse on
    /// completion to call `store.set(key, owned_body, …)` directly,
    /// eliminating the value-bytes memcpy through `Arc::from(&[u8])`.
    Promote {
        total: usize,
        bytes_present: usize,
        /// Index into `tail` where the big-value body bytes begin
        /// (immediately after the `$<bulklen>\r\n` line). Always
        /// `header_len_in_tail = body_start_in_tail`.
        ///
        /// v1.29 C1 — consumed by the C2 BigArgState refactor + C3
        /// bare-SET fast path. Currently dead at the C1 commit point;
        /// kept declared to lock the public-shape of `Promote` so C2/C3
        /// changes are pure additions (zero retracing of upstream
        /// callers when those land).
        #[allow(dead_code)]
        body_start_in_tail: usize,
        /// The N from `$<N>\r\n` of the big bulk. Always
        /// `body_start_in_tail + body_len + 2 == total`.
        #[allow(dead_code)]
        body_len: usize,
        /// **v1.29 B3** — when the promoted shape is exactly bare
        /// `*3\r\n$3\r\nSET\r\n$<klen>\r\n<key>\r\n$<bodylen>\r\n…`,
        /// `Some((start, end))` is the `tail` byte range of `<key>`.
        /// `None` for SETEX / PSETEX / APPEND / GETSET / MSET — those
        /// shapes still promote but the v1.29.0 fast path is bare-SET
        /// only; broader variants come in v1.29.x.
        #[allow(dead_code)]
        bare_set_key_range: Option<(usize, usize)>,
    },
}

/// Walk an ASCII decimal integer at `buf[i..]` until a non-digit; returns
/// the parsed value and the index just past the last digit. Caps at
/// `usize::MAX / 10` to avoid overflow before reaching a final length.
pub(crate) fn parse_decimal_at(buf: &[u8], mut i: usize) -> Option<(usize, usize)> {
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

/// Parse just the verb bulk (`$<L>\r\n<verb>\r\n`) starting at `i`,
/// returning `(verb_upper, after_verb)`. Verbs are short (≤ 16 bytes
/// for everything in the kevy command table), so we copy into a small
/// stack array and ASCII-uppercase. `None` if the bulk isn't fully
/// present yet or is malformed.
fn probe_verb_bulk(buf: &[u8], start: usize) -> Option<([u8; 16], usize, usize)> {
    if buf.get(start) != Some(&b'$') {
        return None;
    }
    let (len, after_len_digits) = parse_decimal_at(buf, start + 1)?;
    if len == 0 || len > 16 {
        return None;
    }
    if buf.get(after_len_digits..after_len_digits + 2) != Some(b"\r\n") {
        return None;
    }
    let v_start = after_len_digits + 2;
    if buf.len() < v_start + len + 2 {
        return None;
    }
    if buf.get(v_start + len..v_start + len + 2) != Some(b"\r\n") {
        return None;
    }
    let mut verb = [0u8; 16];
    for (i, b) in buf[v_start..v_start + len].iter().enumerate() {
        verb[i] = b.to_ascii_uppercase();
    }
    Some((verb, len, v_start + len + 2))
}

/// Whether the (upper-cased) verb is one of the variants the generic
/// BigBulk probe is willing to promote. `SET k v EX 10` (the
/// SET-with-trailing-options case) is excluded because the big bulk
/// isn't last — v1.25.x follow-up. Plain `SET k v` (no options, big
/// value as last bulk) is included.
fn generic_bigbulk_verb_supported(verb: &[u8]) -> bool {
    matches!(
        verb,
        b"SET" | b"APPEND" | b"GETSET" | b"SETEX" | b"PSETEX" | b"MSET"
    )
}

/// Walk a `$<len>\r\n<bytes>\r\n` bulk at `buf[i..]`. If the bulk is
/// fully present, returns `(after_bulk, body_len)` — the byte offset
/// just past the trailing CRLF and the body length.
///
/// If the bulk **header** is present but the body isn't, returns
/// `Err(needed_total)` where `needed_total` is the byte index of the
/// trailing CRLF's last byte (i.e. the slab cursor needed for the bulk
/// to be fully present). This lets the caller pick the "we know how
/// many bytes to wait for" path.
///
/// `None` if the header itself isn't fully present (incomplete probe).
enum BulkStep {
    /// Bulk fully present in buf. `body_start..body_start + body_len` is
    /// the bulk's content range; `after` is the index just past the
    /// trailing CRLF (next bulk's first byte).
    Complete {
        body_start: usize,
        body_len: usize,
        after: usize,
    },
    HeaderOnlyBigBody { body_len: usize, after: usize },
    Incomplete,
}

fn step_bulk(buf: &[u8], i: usize) -> BulkStep {
    if buf.get(i) != Some(&b'$') {
        return BulkStep::Incomplete;
    }
    let Some((body_len, after_len_digits)) = parse_decimal_at(buf, i + 1) else {
        return BulkStep::Incomplete;
    };
    if buf.get(after_len_digits..after_len_digits + 2) != Some(b"\r\n") {
        return BulkStep::Incomplete;
    }
    let body_start = after_len_digits + 2;
    let after = body_start + body_len + 2; // body + trailing CRLF
    if buf.len() >= after {
        // Bulk fully present in slab; verify trailing CRLF.
        if &buf[body_start + body_len..body_start + body_len + 2] != b"\r\n" {
            return BulkStep::Incomplete;
        }
        BulkStep::Complete {
            body_start,
            body_len,
            after,
        }
    } else {
        // Header present, body not (fully) — record the total `after`
        // so the caller knows the frame extent.
        BulkStep::HeaderOnlyBigBody { body_len, after }
    }
}

/// **v1.25 B.5** — generic last-bulk-big probe. See
/// [`BigArgGenericProbe`] for the contract.
///
/// Walks the frame header from byte 0 forward, parsing each bulk's
/// `$<N>\r\n` and skipping past the body (if present in slab) or
/// computing the would-be body extent (if not present). Promotes iff
/// the **last** bulk is the first incomplete one AND its `N ≥
/// BIG_ARG_PROMOTE_THRESHOLD`. A frame where the incomplete bulk is
/// NOT the last bulk (e.g. `SET k <BIG> EX 10` with the big value at
/// position 3 of 5) returns `NotApplicable` — v1.25.x follow-up.
pub(crate) fn probe_generic_bigbulk(buf: &[u8]) -> BigArgGenericProbe {
    if buf.first() != Some(&b'*') {
        return BigArgGenericProbe::NotApplicable;
    }
    let Some((argc, after_argc_digits)) = parse_decimal_at(buf, 1) else {
        return BigArgGenericProbe::NotApplicable;
    };
    if argc < 2 || argc > MAX_PROBE_BULKS {
        return BigArgGenericProbe::NotApplicable;
    }
    if buf.get(after_argc_digits..after_argc_digits + 2) != Some(b"\r\n") {
        return BigArgGenericProbe::NotApplicable;
    }
    let after_argc = after_argc_digits + 2;
    // Verb (bulk 0) must be a supported variant.
    let Some((verb, verb_len, after_verb)) = probe_verb_bulk(buf, after_argc) else {
        return BigArgGenericProbe::NotApplicable;
    };
    if !generic_bigbulk_verb_supported(&verb[..verb_len]) {
        return BigArgGenericProbe::NotApplicable;
    }
    // MSET shape constraint: argc must be odd (verb + N key/value pairs)
    // AND ≥ 3. Anything else is malformed and rejected by the parser
    // later anyway, but we bail here to avoid promoting a doomed frame.
    if verb[..verb_len] == *b"MSET" && (argc < 3 || argc.is_multiple_of(2)) {
        return BigArgGenericProbe::NotApplicable;
    }
    // Walk remaining bulks. The first incomplete bulk MUST be the last
    // bulk (`bulk_idx == argc - 1`) AND must have `N >=
    // BIG_ARG_PROMOTE_THRESHOLD` for promotion.
    let mut cursor = after_verb;
    // v1.29 B3 — capture the key bulk's content range for the bare-SET
    // shape so the dispatch path can call `store.set(key, owned_body, …)`
    // directly without re-parsing the frame on completion.
    let is_bare_set = verb[..verb_len] == *b"SET" && argc == 3;
    let mut key_range: Option<(usize, usize)> = None;
    for bulk_idx in 1..argc {
        match step_bulk(buf, cursor) {
            BulkStep::Complete {
                body_start,
                body_len,
                after,
            } => {
                if is_bare_set && bulk_idx == 1 {
                    key_range = Some((body_start, body_start + body_len));
                }
                cursor = after;
            }
            BulkStep::HeaderOnlyBigBody { body_len, after } => {
                if bulk_idx != argc - 1 {
                    // Big incomplete bulk is NOT last — out of scope.
                    return BigArgGenericProbe::NotApplicable;
                }
                if body_len < BIG_ARG_PROMOTE_THRESHOLD {
                    return BigArgGenericProbe::NotApplicable;
                }
                if body_len > MAX_BULK_LEN {
                    return BigArgGenericProbe::NotApplicable;
                }
                let total = after;
                let bytes_present = buf.len();
                // body_start_in_tail = `after - body_len - 2`. That's the
                // index just past the `$<bodylen>\r\n` line — first byte
                // of body.
                let body_start_in_tail = total - body_len - 2;
                return BigArgGenericProbe::Promote {
                    total,
                    bytes_present,
                    body_start_in_tail,
                    body_len,
                    bare_set_key_range: if is_bare_set { key_range } else { None },
                };
            }
            BulkStep::Incomplete => {
                // Even the bulk HEADER isn't fully present — can't
                // compute frame extent, bail.
                return BigArgGenericProbe::NotApplicable;
            }
        }
    }
    // All bulks fully present already — no point promoting (the regular
    // dispatch path will handle the complete frame).
    BigArgGenericProbe::NotApplicable
}

#[cfg(test)]
#[path = "uring_bigbulk_probe_tests.rs"]
mod tests;
