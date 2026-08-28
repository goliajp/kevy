//! [`Part`] — the partial result one shard ships back to the shard that
//! took the command.
//!
//! Split out of [`crate::message`] under the 500-LOC house rule, the same
//! way [`crate::message_agg`] took [`Agg`](crate::message::Agg) out of it.
//! One enum per file where the enum is large enough to be its own subject.

use crate::message::SmallReply;
use crate::message_kinds::Gathered;

/// A partial result shipped back to the originating shard.
pub(crate) enum Part {
    /// PREFIX.STATS per-shard result.
    PrefixStats { keys: u64, expires: u64 },
    /// Extension fan-out per-shard chunk (opaque to the runtime).
    ExtensionChunk(Vec<u8>),
    /// `REPL.TOKEN` per-shard answer: the answering shard's
    /// id + its live `(feed generation, next_offset)` pair.
    ReplToken { shard: u32, generation: u64, next_offset: u64 },
    Reply(SmallReply),
    Int(i64),
    Ok,
    /// Per-key gathered payloads.
    Gathered(Vec<(Vec<u8>, Gathered)>),
    /// Geo `*STORE` step-1 result: the members the search matched on the
    /// source key's shard (or the error it raised there).
    GeoHits(crate::GeoHits),
    /// A shard's collected keys (KEYS).
    Keys(Vec<Vec<u8>>),
    /// This shard's RANDOMKEY candidate. `live` is the shard's key count — the
    /// reservoir weight — and `draw` is a fresh draw from the shard's own RNG,
    /// so the origin's fold can pick WITHOUT owning an entropy source. Before
    /// this, the reducer took the first shard's candidate every time: a key on
    /// any other shard could never be returned at all.
    RandomKey {
        key: Option<Vec<u8>>,
        live: u64,
        draw: u64,
    },
    /// One shard's `SCAN` page ([`Op::ScanStep`] reply): the next
    /// in-shard cursor (0 = this shard is exhausted), the keys that
    /// passed the filters, and how many buckets the walk visited (the
    /// origin debits its COUNT work budget with it).
    ScanPage {
        next: u64,
        keys: Vec<Vec<u8>>,
        visited: usize,
    },
    /// `WATCH` partial reply: each key this shard owns paired with its
    /// current version, in request order. The origin shard collates
    /// these into the conn's watched set.
    WatchVersions(Vec<(Vec<u8>, u64)>),
    /// Cross-shard RENAME step 1 success: src removed; here's the
    /// value + TTL for the orchestrator to ship into step 2.
    RenameTaken {
        value: kevy_store::Value,
        ttl_ms: Option<u64>,
    },
    /// Cross-shard RENAME step 1 miss: src didn't exist.
    RenameNoSuchSrc,
    /// Cross-shard RENAME step 2 result. `refused` is `None` when the put
    /// landed at dst; `Some((value, ttl))` when `RENAMENX` blocked because
    /// dst already had an entry — the source value (taken in step 1) is
    /// handed back so the orchestrator can put it back on its shard (no
    /// data loss) before replying `:0`.
    RenamePutDone {
        refused: Option<(kevy_store::Value, Option<u64>)>,
    },
    /// Cross-shard COPY step 1: the clone of the source's value and
    /// remaining TTL, or `None` when the source does not exist.
    CopyRead(Option<(kevy_store::Value, Option<u64>)>),
    /// COPY's result, same shape whether one shard answered or two:
    /// `stored` is what the `:1` / `:0` reply reports.
    CopyPutDone { stored: bool },
    /// Cross-shard list move step 1: the popped element, or `None` when the
    /// source was empty/absent. `Err(())` = the source is not a list.
    ListMoveTaken(Result<Option<Vec<u8>>, ()>),
    /// Cross-shard list move step 2: `refused` is `None` when the push
    /// landed. `Some(value)` when the destination exists and is not a list
    /// — the element comes back so the orchestrator can restore the source.
    ListMovePushed { refused: Option<Vec<u8>> },
    /// `SLOWLOG GET` partial: this shard's ring buffer contents (in
    /// FIFO order — oldest first). Origin sorts by timestamp DESC and
    /// truncates per the `Get(count)` request.
    SlowlogEntries(Vec<crate::exec_slowlog::SlowlogEntry>),
    /// One stream's result for a cross-shard `XREAD` gather (see
    /// [`Op::XReadOne`]). `element` is the encoded `*2 <key> <entries>`
    /// reply element (the `*1\r\n` wrapper already stripped) when the
    /// stream had data, or `None` when empty. `index` preserves request
    /// order; an error reply is carried verbatim in `element` and detected
    /// by the leading `-`.
    XReadElement { index: u32, element: Option<Vec<u8>> },
}
