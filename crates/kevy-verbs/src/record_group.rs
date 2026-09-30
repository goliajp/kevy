//! How a group's position, its read counter and a consumer it made are
//! recorded, in forms a 6.4 reader also takes: 6.4 knows `XGROUP CREATE
//! key group id [MKSTREAM]`, `XGROUP SETID key group id` and `XGROUP
//! CREATECONSUMER`, and refuses `ENTRIESREAD` and the internal frames. So
//! the read counter goes in a frame of its own, which 6.4 skips while it
//! still has the group where it stands, and a consumer a command made is
//! recorded as made before its times are.

use kevy_resp::{Argv, ArgvView};
use kevy_store::Store;

/// Where the group stands now: `XGROUP SETID key group <last-delivered>`,
/// which leaves the read counter unknown, then the counter when it is
/// known. Nothing when the group is gone.
pub(crate) fn push_setid_frames(frames: &mut Vec<Argv>, store: &Store, key: &[u8], group: &[u8]) {
    let Some(g) = store.stream_group_peek(key, group) else {
        return;
    };
    let id = g.last_delivered_id().encode();
    let mut f = Argv::with_capacity(5, 0);
    for part in [&b"XGROUP"[..], b"SETID", key, group, &id] {
        f.push(part);
    }
    frames.push(f);
    if g.entries_read().is_some() {
        frames.extend(entries_read_frame(store, key, group));
    }
}

/// `XGROUP SETID key group <last-delivered> ENTRIESREAD <n|-1>`: the
/// group's position and read counter as they stand. `None` when the group
/// is gone.
fn entries_read_frame(store: &Store, key: &[u8], group: &[u8]) -> Option<Argv> {
    let g = store.stream_group_peek(key, group)?;
    let read = g.entries_read().map_or_else(|| "-1".to_owned(), |n| n.to_string());
    let mut f = Argv::with_capacity(7, 0);
    for part in
        [&b"XGROUP"[..], b"SETID", key, group, &g.last_delivered_id().encode(), b"ENTRIESREAD"]
    {
        f.push(part);
    }
    f.push(read.as_bytes());
    Some(f)
}

/// An `XGROUP CREATE` or `SETID` that set the read counter: the command
/// without `ENTRIESREAD n`, then the counter as it left it.
pub(crate) fn group_frames<A: ArgvView + ?Sized>(store: &Store, args: &A) -> Vec<Argv> {
    let mut plain = Argv::with_capacity(args.len(), 0);
    let mut i = 0;
    while i < args.len() {
        if i >= 5 && args[i].eq_ignore_ascii_case(b"ENTRIESREAD") {
            i += 2;
            continue;
        }
        plain.push(&args[i]);
        i += 1;
    }
    let mut frames = vec![plain];
    frames.extend(entries_read_frame(store, &args[2], &args[3]));
    frames
}

/// `XGROUP CREATECONSUMER key group consumer`.
pub(crate) fn create_consumer_frame(key: &[u8], group: &[u8], consumer: &[u8]) -> Argv {
    let mut f = Argv::with_capacity(5, 0);
    for part in [&b"XGROUP"[..], b"CREATECONSUMER", key, group, consumer] {
        f.push(part);
    }
    f
}
