//! Change-feed (CDC): `FEED.SHARDS` / `FEED.TAIL` / `FEED.READ`
//! — the network face of kevy-embedded's `changes_since`.
//!
//! Both backends serve the same cursor contract and hand back the same
//! types, kevy-embedded's: read from a [`FeedPosition`], get a
//! [`ChangeBatch`] of [`Change`]s plus the next cursor; an
//! unservable cursor (stale generation / evicted offsets) surfaces as
//! an error whose message starts with `FEEDRESYNC <gen> <tail>` —
//! rebuild from a scan, then resume from that cursor. The embedded
//! backend is single-shard (shard `0`) and requires the store to be
//! opened with `Config::with_feed`; without it, calls answer
//! `Unsupported`.

use crate::{KevyError, KevyResult};

use kevy_embedded::{Change, ChangeBatch, FeedError, FeedPosition};
use kevy_resp::Reply;
use kevy_resp_client::RespClient;

use crate::{Connection, string, unexpected};

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

    /// `FEED.TAIL shard` — the shard's current cursor: where a consumer
    /// starting fresh (or resuming after a rebuild) begins.
    ///
    /// ```
    /// use kevy_client::Connection;
    /// // mem:// opens its store without a change feed
    /// let mut c = Connection::connect("mem://feed-tail-doc")?;
    /// assert!(c.feed_tail(0).is_err());
    /// # Ok::<(), kevy_client::KevyError>(())
    /// ```
    pub fn feed_tail(&mut self, shard: usize) -> KevyResult<FeedPosition> {
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
                        match (
                            it.next().expect("the items.len() check above"),
                            it.next().expect("the items.len() check above"),
                        ) {
                            (Reply::Int(g), Reply::Int(o)) => {
                                Ok(FeedPosition::new(g as u64, o as u64))
                            }
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
    /// deliver up to `count` changes (server default 256) past `from`,
    /// optionally key-prefix-filtered (fail-open: changes whose key
    /// layout the filter can't cheaply determine are always delivered).
    /// Resume from `batch.next`.
    ///
    /// ```
    /// use kevy_client::{Connection, FeedPosition};
    /// // mem:// opens its store without a change feed
    /// let mut c = Connection::connect("mem://feed-read-doc")?;
    /// assert!(c.feed_read(0, FeedPosition::new(1, 0), None, &[]).is_err());
    /// # Ok::<(), kevy_client::KevyError>(())
    /// ```
    pub fn feed_read(
        &mut self,
        shard: usize,
        from: FeedPosition,
        count: Option<usize>,
        prefixes: &[&[u8]],
    ) -> KevyResult<ChangeBatch> {
        match self {
            Self::Embedded(s) => {
                check_embedded_shard(shard)?;
                s.changes_since(from, count.unwrap_or(256), prefixes).map_err(feed_err)
            }
            Self::Remote(c) => parse_batch(feed_read_request(c, shard, from, count, prefixes)?),
        }
    }
}

/// Embedded feed is single-shard; any other index is a caller bug.
fn check_embedded_shard(shard: usize) -> KevyResult<()> {
    if shard != 0 {
        return Err(KevyError::InvalidInput(
            "embedded feed is single-shard: shard must be 0".into(),
        ));
    }
    Ok(())
}

/// Map [`FeedError`] onto the same error text the wire produces, so
/// resync handling code is backend-agnostic.
fn feed_err(e: FeedError) -> KevyError {
    match e {
        FeedError::Resync { tail } => {
            KevyError::Protocol(format!("FEEDRESYNC {} {}", tail.generation, tail.offset))
        }
        FeedError::Future => KevyError::Protocol("ERR feed cursor ahead of stream".into()),
        FeedError::Disabled => KevyError::Unsupported(
            "feed disabled: open the embedded store with Config::with_feed".into(),
        ),
        // a refusal this version cannot name still reaches the caller
        // with its own text
        other => KevyError::Protocol(format!("ERR feed: {other}")),
    }
}

fn feed_read_request(
    c: &mut RespClient,
    shard: usize,
    from: FeedPosition,
    count: Option<usize>,
    prefixes: &[&[u8]],
) -> KevyResult<Reply> {
    let mut args: Vec<Vec<u8>> = vec![
        b"FEED.READ".to_vec(),
        shard.to_string().into_bytes(),
        from.generation.to_string().into_bytes(),
        from.offset.to_string().into_bytes(),
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
fn parse_batch(reply: Reply) -> KevyResult<ChangeBatch> {
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
    let (Reply::Int(g), Reply::Int(next)) = (
        it.next().expect("the items.len() check above"),
        it.next().expect("the items.len() check above"),
    ) else {
        return Err(KevyError::Protocol("FEED.READ: non-integer cursor".into()));
    };
    let Reply::Array(raw_frames) = it.next().expect("the items.len() check above") else {
        return Err(KevyError::Protocol("FEED.READ: frames not an array".into()));
    };
    let changes = raw_frames.into_iter().map(parse_frame).collect::<KevyResult<_>>()?;
    Ok(ChangeBatch::new(changes, FeedPosition::new(g as u64, next as u64)))
}

fn parse_frame(frame: Reply) -> KevyResult<Change> {
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
    Ok(Change::new(off as u64, argv))
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
        let err = c.feed_read(0, FeedPosition::new(1, 0), None, &[]).unwrap_err();
        assert!(matches!(err, KevyError::Unsupported(_)));
    }

    #[test]
    fn embedded_feed_refusals_carry_the_wire_text() {
        let resync = feed_err(FeedError::Resync { tail: FeedPosition::new(3, 17) });
        assert!(matches!(&resync, KevyError::Protocol(t) if t == "FEEDRESYNC 3 17"), "{resync:?}");
        let future = feed_err(FeedError::Future);
        assert!(
            matches!(&future, KevyError::Protocol(t) if t == "ERR feed cursor ahead of stream"),
            "{future:?}"
        );
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
        assert_eq!(batch.next, FeedPosition::new(1, 42));
        assert_eq!(batch.changes.len(), 1);
        assert_eq!(batch.changes[0].offset, 41);
        assert_eq!(batch.changes[0].argv[0], b"SET");
    }
}
