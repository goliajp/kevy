//! The host-mediated persistence pump. The browser has no filesystem,
//! so the engine never touches storage itself: writes (when capture is
//! on) queue their AOF frames here, the host drains them into whatever
//! it has (OPFS, IndexedDB), and feeds the stored log back on the next
//! open. Frames are `kevy-persist` AOF format byte-for-byte — a log
//! pumped out of a browser tab replays in a native kevy unchanged.
//!
//! The pump speaks both formats: a stored log starting with
//! `KEVYAOF2` replays as checksummed v2 records; anything else (a
//! `KEVYAOF1` header or bare RESP frames from a pre-4.0 tab) replays
//! as v1 — read forever, same contract as the native open. Outbound
//! frames encode in the stored log's format so the host's verbatim
//! appends never mix formats; the first compaction
//! ([`kevy_aof_dump`]) upgrades a v1 log to v2, mirroring the native
//! first-rewrite upgrade.

use crate::{BAD_HANDLE, Instance, arg, with};

/// Feed a chunk of AOF bytes back into the store (the read half of the
/// pump, called during startup replay). Chunks may split frames or
/// records at any byte: an incomplete tail is carried over to the next
/// call. A 9-byte `KEVYAOF2` / `KEVYAOF1` magic header at the start of
/// the stream selects the format (absent = v1 bare frames). Returns
/// the number of frames applied by this call, or -1 on a corrupt frame
/// (message in the result buffer; the corrupt tail is discarded,
/// frames before it were applied).
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

/// Serialize the whole keyspace into a compacted AOF image (v2, magic
/// header included) in the result buffer; returns its byte length. The
/// host **replaces** its stored log with this image — the browser-side
/// equivalent of an AOF rewrite, keeping storage proportional to the
/// live keyspace instead of the write history. Like the native first
/// rewrite, this is the point where a v1-era log upgrades to v2:
/// subsequent outbound frames encode as v2 records.
///
/// Pending un-drained outbound frames are discarded: the image already
/// captures their effects, so appending them after the swap would
/// double-apply on the next replay.
#[unsafe(no_mangle)]
pub extern "C" fn kevy_aof_dump(h: u32) -> i32 {
    with(h, BAD_HANDLE, |inst| {
        inst.aof_out.clear();
        inst.aof_format = kevy_persist::AofFormat::V2;
        inst.aof_out_started = true; // the image carries the magic
        inst.out = inst.store.dump_aof_buf();
        inst.out.len() as i32
    })
}

/// Parse-and-apply loop behind [`kevy_aof_frame_in`].
fn feed(inst: &mut Instance, chunk: &[u8]) -> i32 {
    inst.aof_in_carry.extend_from_slice(chunk);
    if !inst.aof_in_started {
        let v1 = kevy_persist::AOF_MAGIC;
        let v2 = kevy_persist::AOF2_MAGIC;
        let head = &inst.aof_in_carry;
        if head.len() < v2.len() && (v1.starts_with(head.as_slice()) || v2.starts_with(head.as_slice())) {
            // Could still be a magic prefix — wait for more bytes.
            return 0;
        }
        if head.starts_with(v2) {
            inst.aof_format = kevy_persist::AofFormat::V2;
            inst.aof_in_carry.drain(..v2.len());
        } else {
            // KEVYAOF1 header, or bare pre-4.0 frames: the v1
            // read-forever path. Outbound frames follow suit so the
            // host's appends keep the stored log single-format.
            inst.aof_format = kevy_persist::AofFormat::V1;
            if inst.aof_in_carry.starts_with(v1) {
                inst.aof_in_carry.drain(..v1.len());
            }
        }
        // Whatever the format, the host HAS a log — never prepend a
        // fresh-log magic to outbound frames.
        inst.aof_out_started = true;
        inst.aof_in_started = true;
    }
    match inst.aof_format {
        kevy_persist::AofFormat::V1 => feed_v1(inst),
        kevy_persist::AofFormat::V2 => feed_v2(inst),
    }
}

/// v1 body: bare RESP multibulk frames.
fn feed_v1(inst: &mut Instance) -> i32 {
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

/// v2 body: `[len][crc32c][payload]` records; the CRC catches bit-rot
/// the bare-RESP path replayed silently.
fn feed_v2(inst: &mut Instance) -> i32 {
    let mut pos = 0;
    let mut applied = 0i32;
    loop {
        match kevy_persist::next_record(&inst.aof_in_carry, pos) {
            kevy_persist::RecordStep::Ok { payload, consumed } => {
                match kevy_resp::parse_command(payload) {
                    Ok(Some((args, used))) if used == payload.len() => {
                        inst.store.apply_frame(&args);
                        pos += consumed;
                        applied += 1;
                    }
                    _ => {
                        // Checksum passed but the payload is not exactly
                        // one command — a lying record. Same contract as
                        // a CRC failure.
                        inst.aof_in_carry.clear();
                        return inst.fail(format!(
                            "corrupt AOF record (bad payload) after {applied} applied frame(s)"
                        ));
                    }
                }
            }
            // Mid-record end: keep the tail for the next chunk.
            kevy_persist::RecordStep::Truncated => break,
            kevy_persist::RecordStep::Corrupt => {
                inst.aof_in_carry.clear();
                return inst.fail(format!(
                    "corrupt AOF record after {applied} applied frame(s)"
                ));
            }
        }
    }
    inst.aof_in_carry.drain(..pos);
    applied
}
