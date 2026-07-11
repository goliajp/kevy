//! The host-mediated persistence pump. The browser has no filesystem,
//! so the engine never touches storage itself: writes (when capture is
//! on) queue their AOF frames here, the host drains them into whatever
//! it has (OPFS, IndexedDB), and feeds the stored log back on the next
//! open. Frames are `kevy-persist` AOF format byte-for-byte — a log
//! pumped out of a browser tab replays in a native kevy unchanged.

use crate::{BAD_HANDLE, Instance, arg, with};

/// Feed a chunk of AOF bytes back into the store (the read half of the
/// pump, called during startup replay). Chunks may split frames at any
/// byte: an incomplete tail is carried over to the next call. The
/// 9-byte `KEVYAOF1` magic header is consumed if present at the start
/// of the stream. Returns the number of frames applied by this call, or
/// -1 on a corrupt frame (message in the result buffer; the corrupt
/// tail is discarded, frames before it were applied).
///
/// Frames applied here are **not** re-captured into the outbound pump —
/// the host already has them.
///
/// # Safety
///
/// Pointer/length pairs follow the [`crate::arg`] contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kevy_aof_frame_in(h: u32, p: *const u8, l: u32) -> i32 {
    // SAFETY: loader-staged argument buffer, live for this call.
    let chunk = unsafe { arg(p, l) };
    with(h, BAD_HANDLE, |inst| feed(inst, chunk))
}

/// Drain the pending outbound AOF frames into the result buffer (the
/// write half of the pump). Returns the byte length (0 = nothing
/// pending). The host appends these bytes verbatim to its stored log.
#[unsafe(no_mangle)]
pub extern "C" fn kevy_aof_frames_out(h: u32) -> i32 {
    with(h, BAD_HANDLE, |inst| {
        inst.out.clear();
        std::mem::swap(&mut inst.out, &mut inst.aof_out);
        inst.out.len() as i32
    })
}

/// Serialize the whole keyspace into a compacted AOF image (magic
/// header included) in the result buffer; returns its byte length. The
/// host **replaces** its stored log with this image — the browser-side
/// equivalent of an AOF rewrite, keeping storage proportional to the
/// live keyspace instead of the write history.
///
/// Pending un-drained outbound frames are discarded: the image already
/// captures their effects, so appending them after the swap would
/// double-apply on the next replay.
#[unsafe(no_mangle)]
pub extern "C" fn kevy_aof_dump(h: u32) -> i32 {
    with(h, BAD_HANDLE, |inst| {
        inst.aof_out.clear();
        inst.out = inst.store.dump_aof_buf();
        inst.out.len() as i32
    })
}

/// Parse-and-apply loop behind [`kevy_aof_frame_in`].
fn feed(inst: &mut Instance, chunk: &[u8]) -> i32 {
    inst.aof_in_carry.extend_from_slice(chunk);
    if !inst.aof_in_started {
        let magic = kevy_persist::AOF_MAGIC;
        if inst.aof_in_carry.len() < magic.len() && magic.starts_with(&inst.aof_in_carry) {
            // Could still be a magic prefix — wait for more bytes.
            return 0;
        }
        if inst.aof_in_carry.starts_with(magic) {
            inst.aof_in_carry.drain(..magic.len());
        }
        inst.aof_in_started = true;
    }
    let mut pos = 0;
    let mut applied = 0i32;
    loop {
        match kevy_resp::parse_command(&inst.aof_in_carry[pos..]) {
            Ok(Some((args, consumed))) => {
                inst.store.apply_frame(&args);
                pos += consumed;
                applied += 1;
            }
            Ok(None) => break, // incomplete tail — keep for the next chunk
            Err(e) => {
                inst.aof_in_carry.clear();
                return inst.fail(format!(
                    "corrupt AOF frame after {applied} applied frame(s): {e:?}"
                ));
            }
        }
    }
    inst.aof_in_carry.drain(..pos);
    applied
}
