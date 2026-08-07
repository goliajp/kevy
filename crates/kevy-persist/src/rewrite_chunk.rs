//! The rewrite frame chunker — split from `rewrite_fmt.rs` at the
//! 500-LOC line. One seam, one job: turn a collection's item stream
//! into bounded `[verb, key, items…]` frames.

use std::io::{self, Write};

use kevy_resp::Argv;

use crate::rewrite_fmt::emit;

/// Redis's own rewrite batching constant (AOF_REWRITE_ITEMS_PER_CMD):
/// a collection rewrites as MANY `[verb, key, ≤64 items…]` frames, not
/// one giant one. The tailgate storm proved why the cap is load-
/// bearing here: a single multi-GiB value serialized as ONE frame
/// wraps `Argv`'s u32 offset table (`slice index starts at
/// 4294966283…`) and takes the persist thread down with it.
const REWRITE_ITEMS_PER_CMD: usize = 64;

/// A second bound Redis does not need but kevy's `Argv` does: 64 items
/// × the 512 MB proto bulk cap could still pass 4 GiB, so a frame also
/// closes at this many payload bytes — every frame stays far inside
/// the u32 offset space.
const REWRITE_BYTES_PER_CMD: usize = 256 << 20;

/// `[verb, key, items…]` multi-bulk frames — the shared body of every
/// per-type arm above, chunked (see the two constants). `unit` is the
/// indivisible item group: 1 for list/set elements, 2 for the
/// flattened field/value and score/member pairs — a chunk boundary
/// never splits a pair.
pub(crate) fn write_verb_items<W: Write>(
    w: &mut W,
    verb: &[u8],
    key: &[u8],
    unit: usize,
    items: impl IntoIterator<Item = Vec<u8>>,
    fmt: crate::AofFormat,
    scratch: &mut Vec<u8>,
) -> io::Result<()> {
    let unit = unit.max(1);
    let mut it = items.into_iter().peekable();
    // An empty collection emits nothing (it cannot exist in the store;
    // SET/PEXPIREAT always carry one item).
    while it.peek().is_some() {
        let mut argv: Vec<Vec<u8>> = Vec::with_capacity(2 + REWRITE_ITEMS_PER_CMD * unit);
        argv.push(verb.to_vec());
        argv.push(key.to_vec());
        let mut bytes = 0usize;
        let mut units = 0usize;
        while units < REWRITE_ITEMS_PER_CMD && bytes < REWRITE_BYTES_PER_CMD {
            if it.peek().is_none() {
                break;
            }
            for _ in 0..unit {
                let Some(item) = it.next() else { break };
                bytes += item.len();
                argv.push(item);
            }
            units += 1;
        }
        emit(w, &Argv::from(argv), fmt, scratch)?;
    }
    Ok(())
}

