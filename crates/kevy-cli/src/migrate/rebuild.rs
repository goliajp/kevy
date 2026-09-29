//! Rebuild frames: how one key becomes the DEL + write commands that
//! recreate it, and what is reported when nothing can.

use std::io::{self, Write};

use crate::link::Link;
use kevy_resp::{Reply, encode_command_borrowed};

/// Emit one key's rebuild frames. `Ok(None)` = nothing written; the
/// type name comes back when the reason was "no rebuild verb", so the
/// caller can report what it is leaving behind rather than count it as
/// a key that happened to vanish.
pub(super) fn export_key(
    client: &mut dyn Link,
    key: &[u8],
    out: &mut impl Write,
) -> io::Result<Option<Option<Vec<u8>>>> {
    match rebuild_frames(client, key, key)? {
        Rebuilt::Frames(frame) => {
            out.write_all(&frame)?;
            Ok(Some(None))
        }
        Rebuilt::Vanished => Ok(None),
        Rebuilt::UnsupportedType(ty) => Ok(Some(Some(ty))),
    }
}

/// Why a key produced no frames. The two reasons are not the same and
/// were indistinguishable: a vanished key is a race the walk expects,
/// an unsupported type is data the caller is about to leave behind.
pub(crate) enum Rebuilt {
    /// The rebuild frames, ready to write.
    Frames(Vec<u8>),
    /// The key was gone between SCAN and read — expected, uncounted.
    Vanished,
    /// The type has no rebuild verb here. Carries the type name so the
    /// caller can tell someone rather than skip in silence.
    UnsupportedType(Vec<u8>),
}

/// Read `key` and produce DEL+rebuild frames addressed to `dst`
/// (`dst == key` for export; a re-prefixed name for copy-prefix —
/// the server has no COPY verb, so copying IS read+rebuild).
pub(crate) fn rebuild_frames(client: &mut dyn Link, key: &[u8], dst: &[u8]) -> io::Result<Rebuilt> {
    let ty = match client.request_borrowed(&[b"TYPE", key])? {
        Reply::Simple(t) => t,
        _ => return Ok(Rebuilt::Vanished),
    };
    if ty == b"none" {
        return Ok(Rebuilt::Vanished);
    }
    let mut frame = Vec::new();
    // DEL first: replay rebuilds from scratch (idempotence for
    // append-shaped verbs like RPUSH).
    encode_command_borrowed(&mut frame, &[b"DEL", dst]);
    match encode_body(client, key, dst, &ty, &mut frame)? {
        Some(()) => {}
        None => return Ok(Rebuilt::Vanished),
    }
    if frame.len() == encoded_del_len(dst) {
        return Ok(Rebuilt::UnsupportedType(ty));
    }
    append_ttl_frame(client, key, dst, &mut frame)?;
    Ok(Rebuilt::Frames(frame))
}

/// The `DEL <dst>` prologue's encoded length — how `rebuild_frames`
/// tells "the body wrote nothing" from "the body wrote frames".
fn encoded_del_len(dst: &[u8]) -> usize {
    let mut probe = Vec::new();
    encode_command_borrowed(&mut probe, &[b"DEL", dst]);
    probe.len()
}

/// Append the type's rebuild verbs to `frame`. `None` = the key
/// vanished mid-read; leaving `frame` untouched = no verb for this
/// type, which the caller turns into `UnsupportedType`.
fn encode_body(
    client: &mut dyn Link,
    key: &[u8],
    dst: &[u8],
    ty: &[u8],
    frame: &mut Vec<u8>,
) -> io::Result<Option<()>> {
    match ty {
        b"string" => {
            let Reply::Bulk(v) = client.request_borrowed(&[b"GET", key])? else {
                return Ok(None);
            };
            encode_command_borrowed(frame, &[b"SET", dst, &v]);
        }
        b"hash" => {
            let Some(items) = fetch_bulks(client, &[b"HGETALL", key])? else {
                return Ok(None);
            };
            encode_multi(frame, b"HSET", dst, &items);
        }
        b"list" => {
            let Some(vals) = fetch_bulks(client, &[b"LRANGE", key, b"0", b"-1"])? else {
                return Ok(None);
            };
            encode_multi(frame, b"RPUSH", dst, &vals);
        }
        b"set" => {
            let Some(ms) = fetch_bulks(client, &[b"SMEMBERS", key])? else {
                return Ok(None);
            };
            encode_multi(frame, b"SADD", dst, &ms);
        }
        b"zset" => {
            let zrange: &[&[u8]] = &[b"ZRANGE", key, b"0", b"-1", b"WITHSCORES"];
            let Some(flat) = fetch_bulks(client, zrange)? else {
                return Ok(None);
            };
            encode_zadd(frame, dst, &flat);
        }
        // Streams and anything added later: no rebuild verb here. The
        // caller reports it by name — a migration that drops a type
        // must say which one.
        _ => return Ok(Some(())),
    }
    Ok(Some(()))
}

/// Issue `cmd` and unwrap its Array reply into bulk payloads.
/// `None` when the reply isn't an array or the array is empty (the key
/// vanished / changed type between TYPE and read).
fn fetch_bulks(client: &mut dyn Link, cmd: &[&[u8]]) -> io::Result<Option<Vec<Vec<u8>>>> {
    let Reply::Array(items) = client.request_borrowed(cmd)? else {
        return Ok(None);
    };
    if items.is_empty() {
        return Ok(None);
    }
    Ok(Some(
        items
            .into_iter()
            .filter_map(|r| if let Reply::Bulk(b) = r { Some(b) } else { None })
            .collect(),
    ))
}

/// Encode `<verb> <dst> <vals…>` onto `frame` (HSET / RPUSH / SADD).
fn encode_multi(frame: &mut Vec<u8>, verb: &[u8], dst: &[u8], vals: &[Vec<u8>]) {
    let mut argv: Vec<&[u8]> = vec![verb, dst];
    argv.extend(vals.iter().map(Vec::as_slice));
    encode_command_borrowed(frame, &argv);
}

/// Encode `ZADD <dst> score member …` onto `frame`.
/// ZADD wants score member; ZRANGE gives member score.
fn encode_zadd(frame: &mut Vec<u8>, dst: &[u8], flat: &[Vec<u8>]) {
    let mut argv: Vec<&[u8]> = vec![b"ZADD", dst];
    for pair in flat.chunks(2) {
        if pair.len() == 2 {
            argv.push(&pair[1]);
            argv.push(&pair[0]);
        }
    }
    encode_command_borrowed(frame, &argv);
}

/// TTL rides as an absolute PEXPIREAT follow-up.
fn append_ttl_frame(
    client: &mut dyn Link,
    key: &[u8],
    dst: &[u8],
    frame: &mut Vec<u8>,
) -> io::Result<()> {
    if let Reply::Int(ms) = client.request_borrowed(&[b"PTTL", key])?
        && ms > 0
    {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(io::Error::other)?
            .as_millis() as i64;
        encode_command_borrowed(frame, &[b"PEXPIREAT", dst, (now + ms).to_string().as_bytes()]);
    }
    Ok(())
}
