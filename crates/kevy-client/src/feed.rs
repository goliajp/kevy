//! Change-feed (CDC): `FEED.SHARDS` / `FEED.TAIL` / `FEED.READ`
//! (v1.14.0) — the network face of kevy-embedded's `changes_since`.
//!
//! Both backends serve the same cursor contract: read from
//! `(generation, offset)`, get frames plus the next cursor; an
//! unservable cursor (stale generation / evicted offsets) surfaces as
//! an error whose message starts with `FEEDRESYNC <gen> <tail>` —
//! rebuild from a scan, then resume from that cursor. The embedded
//! backend is single-shard (shard `0`) and requires the store to be
//! opened with `Config::with_feed`; without it, calls answer
//! `Unsupported`.

use crate::{KevyError, KevyResult};

use kevy_embedded::FeedError;
use kevy_resp::Reply;
use kevy_resp_client::RespClient;

use crate::{Connection, string, unexpected};

/// One change frame from [`Connection::feed_read`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedFrame {
    /// Stream offset (monotonic within a generation).
    pub offset: u64,
    /// The applied effect's argv (the same frame the AOF / a replica
    /// sees), e.g. `["SET", "k", "v"]`.
    pub argv: Vec<Vec<u8>>,
}

/// A batch of change frames plus the cursor to resume from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedBatch {
    /// The stream's current generation.
    pub generation: u64,
    /// Offset to pass to the next [`Connection::feed_read`].
    pub next_offset: u64,
    /// Delivered frames, offset order. May be empty (caught up).
    pub frames: Vec<FeedFrame>,
}

impl Connection {
    /// `FEED.SHARDS` — number of change-feed shards (embedded: always 1).
    pub fn feed_shards(&mut self) -> KevyResult<usize> {
        match self {
            Self::Embedded(s) => Ok(s.feed_shards()),
            Self::Remote(c) => match c.request_borrowed(&[b"FEED.SHARDS"])? {
                Reply::Int(n) if n >= 0 => Ok(n as usize),
                Reply::Error(e) => Err(KevyError::Protocol(string(e))),
                other => Err(unexpected(other)),
            },
        }
    }

    /// `FEED.TAIL shard` — the shard's current `(generation,
    /// next_offset)` cursor: where a consumer starting fresh (or
    /// resuming after a rebuild) begins.
    pub fn feed_tail(&mut self, shard: usize) -> KevyResult<(u64, u64)> {
        match self {
            Self::Embedded(s) => {
                check_embedded_shard(shard)?;
                s.changes_tail().map_err(feed_err)
            }
            Self::Remote(c) => {
                let sh = shard.to_string();
                match c.request_borrowed(&[b"FEED.TAIL", sh.as_bytes()])? {
                    Reply::Array(items) if items.len() == 2 => {
                        let mut it = items.into_iter();
                        match (it.next().unwrap(), it.next().unwrap()) {
                            (Reply::Int(g), Reply::Int(o)) => Ok((g as u64, o as u64)),
                            (a, _) => Err(unexpected(a)),
                        }
                    }
                    Reply::Error(e) => Err(KevyError::Protocol(string(e))),
                    other => Err(unexpected(other)),
                }
            }
        }
    }

    /// `FEED.READ shard generation offset [COUNT n] [PREFIX p …]` —
    /// deliver up to `count` frames (server default 256) past the
    /// cursor, optionally key-prefix-filtered (fail-open: frames whose
    /// key layout the filter can't cheaply determine are always
    /// delivered). Resume from `(batch.generation, batch.next_offset)`.
    pub fn feed_read(
        &mut self,
        shard: usize,
        generation: u64,
        offset: u64,
        count: Option<usize>,
        prefixes: &[&[u8]],
    ) -> KevyResult<FeedBatch> {
        match self {
            Self::Embedded(s) => {
                check_embedded_shard(shard)?;
                let batch = s
                    .changes_since(generation, offset, count.unwrap_or(256), prefixes)
                    .map_err(feed_err)?;
                Ok(FeedBatch {
                    generation: batch.next.0,
                    next_offset: batch.next.1,
                    frames: batch
                        .changes
                        .into_iter()
                        .map(|ch| FeedFrame { offset: ch.offset, argv: ch.argv })
                        .collect(),
                })
            }
            Self::Remote(c) => {
                parse_batch(feed_read_request(c, shard, generation, offset, count, prefixes)?)
            }
        }
    }
}

/// Embedded feed is single-shard; any other index is a caller bug.
fn check_embedded_shard(shard: usize) -> KevyResult<()> {
    if shard != 0 {
        return Err(KevyError::InvalidInput("embedded feed is single-shard: shard must be 0".into()));
    }
    Ok(())
}

/// Map [`FeedError`] onto the same error text the wire produces, so
/// resync handling code is backend-agnostic.
fn feed_err(e: FeedError) -> KevyError {
    match e {
        FeedError::Resync { generation, tail } => {
            KevyError::Protocol(format!("FEEDRESYNC {generation} {tail}"))
        }
        FeedError::Future => KevyError::Protocol("ERR feed cursor ahead of stream".into()),
        FeedError::Disabled => KevyError::Unsupported("feed disabled: open the embedded store with Config::with_feed".into()),
    }
}

fn feed_read_request(
    c: &mut RespClient,
    shard: usize,
    generation: u64,
    offset: u64,
    count: Option<usize>,
    prefixes: &[&[u8]],
) -> KevyResult<Reply> {
    let mut args: Vec<Vec<u8>> = vec![
        b"FEED.READ".to_vec(),
        shard.to_string().into_bytes(),
        generation.to_string().into_bytes(),
        offset.to_string().into_bytes(),
    ];
    if let Some(n) = count {
        args.push(b"COUNT".to_vec());
        args.push(n.to_string().into_bytes());
    }
    for p in prefixes {
        args.push(b"PREFIX".to_vec());
        args.push(p.to_vec());
    }
    Ok(c.request(&args)?)
}

/// `*3 [:generation, :next_offset, *N frames]`, each frame
/// `*2 [:offset, *M argv]`.
fn parse_batch(reply: Reply) -> KevyResult<FeedBatch> {
    let Reply::Array(items) = reply else {
        return match reply {
            Reply::Error(e) => Err(KevyError::Protocol(string(e))),
            other => Err(unexpected(other)),
        };
    };
    if items.len() != 3 {
        return Err(KevyError::Protocol("FEED.READ: expected [gen, next, frames]".into()));
    }
    let mut it = items.into_iter();
    let (Reply::Int(g), Reply::Int(next)) = (it.next().unwrap(), it.next().unwrap()) else {
        return Err(KevyError::Protocol("FEED.READ: non-integer cursor".into()));
    };
    let Reply::Array(raw_frames) = it.next().unwrap() else {
        return Err(KevyError::Protocol("FEED.READ: frames not an array".into()));
    };
    let frames = raw_frames
        .into_iter()
        .map(parse_frame)
        .collect::<KevyResult<_>>()?;
    Ok(FeedBatch { generation: g as u64, next_offset: next as u64, frames })
}

fn parse_frame(frame: Reply) -> KevyResult<FeedFrame> {
    let Reply::Array(cells) = frame else {
        return Err(KevyError::Protocol("FEED.READ: frame not an array".into()));
    };
    let mut it = cells.into_iter();
    let (Some(Reply::Int(off)), Some(Reply::Array(argv_raw))) = (it.next(), it.next()) else {
        return Err(KevyError::Protocol("FEED.READ: frame shape != [offset, argv]".into()));
    };
    let argv = argv_raw
        .into_iter()
        .map(|a| match a {
            Reply::Bulk(b) | Reply::Simple(b) => Ok(b),
            other => Err(unexpected(other)),
        })
        .collect::<KevyResult<_>>()?;
    Ok(FeedFrame { offset: off as u64, argv })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_without_feed_config_is_unsupported() {
        // mem:// opens the store without Config::with_feed.
        let mut c = Connection::connect("mem://").unwrap();
        assert_eq!(c.feed_shards().unwrap(), 1);
        let err = c.feed_tail(0).unwrap_err();
        assert!(matches!(err, KevyError::Unsupported(_)));
        let err = c.feed_read(0, 1, 0, None, &[]).unwrap_err();
        assert!(matches!(err, KevyError::Unsupported(_)));
    }

    #[test]
    fn embedded_nonzero_shard_rejected() {
        let mut c = Connection::connect("mem://").unwrap();
        let err = c.feed_tail(1).unwrap_err();
        assert!(matches!(err, KevyError::InvalidInput(_)));
    }

    #[test]
    fn batch_parser_maps_frames() {
        let reply = Reply::Array(vec![
            Reply::Int(1),
            Reply::Int(42),
            Reply::Array(vec![Reply::Array(vec![
                Reply::Int(41),
                Reply::Array(vec![
                    Reply::Bulk(b"SET".to_vec()),
                    Reply::Bulk(b"k".to_vec()),
                    Reply::Bulk(b"v".to_vec()),
                ]),
            ])]),
        ]);
        let batch = parse_batch(reply).unwrap();
        assert_eq!(batch.generation, 1);
        assert_eq!(batch.next_offset, 42);
        assert_eq!(batch.frames.len(), 1);
        assert_eq!(batch.frames[0].offset, 41);
        assert_eq!(batch.frames[0].argv[0], b"SET");
    }
}
