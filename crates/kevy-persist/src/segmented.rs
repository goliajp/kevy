//! The `SEGMENTED` internal frame — the AOF's stitch to the cold
//! segment tier. Rides the ordinary record envelope as a two-element
//! multibulk, NUL-prefixed like the transaction markers so no RESP
//! verb a client can type ever collides with it.
//!
//! # What the frame means — and what it does not
//!
//! `[SEGMENTED, <seg-file>]` in a shard's AOF says: "every row this
//! segment holds had been evicted from the hot layer at this point in
//! the log". Replay uses it to re-do that eviction — the rows' SET
//! frames precede it, replay them in and this frame asks them back
//! out. The frame is a *timing stitch inside the log stream*, not the
//! segment set's source of truth: the truth about which segments are
//! live is the segment manifest, fsynced BEFORE this frame is
//! appended. Two consequences, both load-bearing:
//!
//! - A frame naming a segment the manifest does not hold means the
//!   truth set was damaged after the fact (the manifest is written
//!   first) — the rows' only durable copy is unreachable, so startup
//!   refuses by name rather than silently dropping rows.
//! - A frame LOST to a snapshot truncation or an AOF rewrite is
//!   harmless: by then the hot layer no longer holds the evicted rows
//!   (the eviction preceded the SAVE/rewrite), or — if a rewrite view
//!   froze mid-eviction — the rows survive in both tiers and
//!   hot-first reads shadow the segment copy. Rewrite therefore needs
//!   no seam for this frame.

use kevy_resp::ArgvView;

/// The frame's verb. The leading NUL is what makes it internal: no
/// client-typed RESP verb can start with it.
pub const SEGMENTED: &[u8] = b"\0KEVYSEGMENTED";

/// If `args` is a `SEGMENTED` frame, its segment file name. Replay
/// drivers check this before ordinary dispatch — an unrecognized
/// internal frame must never fall through as a silent unknown verb.
pub fn segmented_frame<A: ArgvView + ?Sized>(args: &A) -> Option<&[u8]> {
    (args.len() == 2 && args.get(0) == Some(SEGMENTED)).then(|| args.get(1)).flatten()
}

/// The `SEGMENTED` frame as an argv, ready for the record writer.
pub fn segmented_argv(seg_file: &[u8]) -> [&[u8]; 2] {
    [SEGMENTED, seg_file]
}
