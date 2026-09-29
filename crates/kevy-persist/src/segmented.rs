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
///
/// ```
/// assert_eq!(kevy_persist::SEGMENTED[0], 0);
/// assert!(kevy_persist::SEGMENTED.ends_with(b"KEVYSEGMENTED"));
/// ```
pub const SEGMENTED: &[u8] = b"\0KEVYSEGMENTED";

/// If `args` is a `SEGMENTED` frame, its segment file name. Replay
/// drivers check this before ordinary dispatch — an unrecognized
/// internal frame must never fall through as a silent unknown verb.
///
/// ```
/// use kevy_persist::{Argv, segmented_argv, segmented_frame};
///
/// let mut frame = Argv::default();
/// for part in segmented_argv(b"seg-7.kseg") {
///     frame.push(part);
/// }
/// assert_eq!(segmented_frame(&frame), Some(&b"seg-7.kseg"[..]));
/// let set = Argv::from(vec![b"SET".to_vec(), b"k".to_vec()]);
/// assert_eq!(segmented_frame(&set), None);
/// ```
pub fn segmented_frame<A: ArgvView + ?Sized>(args: &A) -> Option<&[u8]> {
    (args.len() == 2 && args.get(0) == Some(SEGMENTED)).then(|| args.get(1)).flatten()
}

/// The `SEGMENTED` frame as an argv, ready for the record writer.
///
/// ```
/// use kevy_persist::{Argv, RecordStep, SEGMENTED, next_record, segmented_argv};
///
/// let mut frame = Argv::default();
/// for part in segmented_argv(b"seg-7.kseg") {
///     frame.push(part);
/// }
/// let mut log = Vec::new();
/// kevy_persist::write_record_multibulk(&mut log, &frame, &mut Vec::new())?;
/// let RecordStep::Ok { payload, .. } = next_record(&log, 0) else { panic!("valid record") };
/// assert!(payload.windows(SEGMENTED.len()).any(|w| w == SEGMENTED));
/// # Ok::<(), std::io::Error>(())
/// ```
pub fn segmented_argv(seg_file: &[u8]) -> [&[u8]; 2] {
    [SEGMENTED, seg_file]
}

use std::io::{self, Write};

/// The rewrite's trailing SEGMENTED frames: one per segment the
/// frozen entries reference. Replay rebuilds the hot stream first,
/// then each frame re-establishes its rows' stubs — rows the stream
/// never carried are inserted straight from the segment.
pub(crate) fn write_segmented_frames<W: Write, S: crate::SnapshotSource>(
    w: &mut W,
    src: &S,
    cold_seqs: &[u32],
    scratch: &mut Vec<u8>,
) -> io::Result<()> {
    if cold_seqs.is_empty() {
        return Ok(());
    }
    let files = src.row_seg_files();
    for seq in cold_seqs {
        let file =
            files.iter().find(|(q, _)| q == seq).map(|(_, f)| f.clone()).ok_or_else(|| {
                io::Error::other(format!("rewrite: stub references unknown segment seq {seq}"))
            })?;
        let argv = segmented_argv(file.as_bytes());
        crate::record::write_record_multibulk(&mut *w, &Frame2(argv), scratch)?;
    }
    Ok(())
}

/// Two borrowed parts as an ArgvView (the SEGMENTED frame's shape).
struct Frame2<'a>([&'a [u8]; 2]);
impl core::ops::Index<usize> for Frame2<'_> {
    type Output = [u8];
    fn index(&self, i: usize) -> &[u8] {
        self.0[i]
    }
}
impl kevy_resp::ArgvView for Frame2<'_> {
    fn len(&self) -> usize {
        2
    }
    fn get(&self, i: usize) -> Option<&[u8]> {
        self.0.get(i).copied()
    }
}
