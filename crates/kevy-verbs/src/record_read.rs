//! What an `XREADGROUP` is recorded as.
//!
//! A read of new entries (`>`) stamps each entry it delivers with the
//! clock. Replayed as typed, the same read delivers the same entries (the
//! state picks them) but stamps them with the replay's clock, so every
//! pending entry would look freshly delivered after a restart. So each
//! stream read with `>` is recorded as what it left:
//!
//! * the entries it put in the pending list: one `XCLAIM key group
//!   consumer 0 id… TIME t RETRYCOUNT n FORCE JUSTID` per `(delivery time,
//!   delivery count)` they hold, the frame a claim is recorded with.
//!   `FORCE` makes the rows, which the replay has not yet made;
//! * how far it moved the group: `XGROUP SETID key group <last-delivered>`.
//!
//! With `NOACK` no pending entries are made, so only the `SETID` frame is
//! recorded. A read of history (an explicit ID) changes no pending entry
//! and moves nothing.
//!
//! Every read, of any form and whatever it finds, is the consumer's
//! latest contact with the group, and creates the consumer if missing. So
//! each stream's frames start with `XGROUP CREATECONSUMER key group
//! consumer TIME t`, `t` that contact: the frames after it find the
//! consumer and leave its time be.

use std::ops::Bound;

use kevy_resp::{Argv, ArgvView};
use kevy_store::{Store, StreamId};

use crate::record::{seen_frame, taken_frames};

/// The frames for an `XREADGROUP` `args` just run, `marks` holding, per
/// stream in `STREAMS` order, the group's last-delivered ID before the
/// read.
pub(crate) fn read_frames<A: ArgvView + ?Sized>(
    store: &Store,
    args: &A,
    marks: &[StreamId],
) -> Vec<Argv> {
    let Some(shape) = Shape::of(args) else { return Vec::new() };
    let (group, consumer) = (&args[2], &args[3]);
    let mut frames = Vec::new();
    for (k, prev) in marks.iter().enumerate().take(shape.streams) {
        let key = &args[shape.keys + k];
        frames.extend(seen_frame(store, key, group, consumer));
        let mut claims = Vec::new();
        if &args[shape.keys + shape.streams + k] == b">"
            && let Some(g) = store.stream_group_peek(key, group)
            && g.last_delivered_id != *prev
        {
            let last = g.last_delivered_id;
            if !shape.noack {
                let span = (Bound::Excluded(*prev), Bound::Included(last));
                let ids: Vec<StreamId> = g.pel.range(span).map(|(id, _)| *id).collect();
                claims = taken_frames(store, key, group, consumer, &ids);
            }
            let mut setid = Argv::with_capacity(5, 0);
            for part in [&b"XGROUP"[..], b"SETID", key, group, &last.encode()] {
                setid.push(part);
            }
            frames.push(setid);
        }
        frames.extend(claims);
    }
    frames
}

/// Where an `XREADGROUP`'s parts sit.
struct Shape {
    noack: bool,
    /// The first stream key.
    keys: usize,
    streams: usize,
}

impl Shape {
    /// `XREADGROUP GROUP g c [COUNT n] [BLOCK ms] [NOACK] STREAMS key… id…`;
    /// `None` for anything else, which ran as a refusal.
    fn of<A: ArgvView + ?Sized>(args: &A) -> Option<Shape> {
        let mut noack = false;
        let mut i = 4;
        while i < args.len() {
            let tok = &args[i];
            if tok.eq_ignore_ascii_case(b"STREAMS") {
                let rest = args.len() - i - 1;
                return (rest > 0 && rest.is_multiple_of(2)).then_some(Shape {
                    noack,
                    keys: i + 1,
                    streams: rest / 2,
                });
            }
            if tok.eq_ignore_ascii_case(b"NOACK") {
                noack = true;
                i += 1;
            } else {
                i += 2;
            }
        }
        None
    }
}
