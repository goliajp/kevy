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
//! * how far it moved the group: `XGROUP SETID key group
//!   <last-delivered>`, and the read counter it left when that is known:
//!   the same frame again with `ENTRIESREAD n`.
//!
//! With `NOACK` no pending entries are made, so no `XCLAIM` frame is
//! recorded. A read of history (an explicit ID) moves nothing, but each
//! entry it hands back is delivered again: it is recorded like a new
//! delivery, one `XCLAIM … TIME t RETRYCOUNT n FORCE JUSTID` per `(time,
//! count)` the entries hold now.
//!
//! A read is the consumer's latest contact with the group, and creates the
//! consumer if missing. A stream the read delivered from, or made the
//! consumer on, has its frames end with `XINTERNAL.CONSUMERSEEN key group
//! consumer t [a]`, `t` that contact and `a` the last time the consumer
//! was handed an entry: the claim frames before it stamp the consumer with
//! the replay's clock, and it sets both times back. A read that delivered
//! nothing and made no consumer is not recorded at all, so a consumer that
//! only polls comes back from a restart with the contact of its last
//! recorded read.

use std::ops::Bound;

use kevy_resp::{Argv, ArgvView};
use kevy_store::{Store, StreamId};

use crate::record::{Consumer, push_setid_frames, seen_frame, taken_frames};

/// The frames for an `XREADGROUP` `args` just run, `marks` holding, per
/// stream in `STREAMS` order, the group's last-delivered ID before the
/// read and whether the read created the consumer.
pub(crate) fn read_frames<A: ArgvView + ?Sized>(
    store: &Store,
    args: &A,
    marks: &[(StreamId, Consumer)],
    redelivered: &[Vec<StreamId>],
) -> Vec<Argv> {
    let Some(shape) = Shape::of(args) else { return Vec::new() };
    let (group, consumer) = (&args[2], &args[3]);
    let mut frames = Vec::new();
    for (k, (prev, consumer_was)) in marks.iter().enumerate().take(shape.streams) {
        let key = &args[shape.keys + k];
        let mut claims = Vec::new();
        let mut moved = false;
        if &args[shape.keys + shape.streams + k] == b">"
            && let Some(g) = store.stream_group_peek(key, group)
            && g.last_delivered_id() != *prev
        {
            let last = g.last_delivered_id();
            if !shape.noack {
                let span = (Bound::Excluded(*prev), Bound::Included(last));
                let ids: Vec<StreamId> = g.pending_range(span).map(|(id, _)| id).collect();
                claims = taken_frames(store, key, group, consumer, &ids);
            }
            moved = true;
        }
        let again = redelivered.get(k).map_or(&[][..], Vec::as_slice);
        if !again.is_empty() {
            claims.extend(taken_frames(store, key, group, consumer, again));
        }
        let seen = moved || *consumer_was == Consumer::Created || !again.is_empty();
        if moved {
            push_setid_frames(&mut frames, store, key, group);
        }
        frames.extend(claims);
        if seen {
            frames.extend(seen_frame(store, key, group, consumer));
        }
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
