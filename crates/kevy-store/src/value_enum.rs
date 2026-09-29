//! [`Value`], the stored-value enum, and the constants that pick its
//! string encodings. Split from `value.rs` for the 500-LOC house rule.

use crate::value::{ColdRef, HashData, ListData, SetData, SmallBytes, ZSetData};
use alloc::boxed::Box;
use alloc::sync::Arc;

/// A stored value. One variant per Redis type.
///
/// The collection variants live behind a **shared pointer** (`Arc`) so the
/// enum is only as big as `Str` (24 B) + tag = 32 B, not the 56 B `ZSetData`
/// — every `Entry` (incl. the common string case) is then ~48 B instead of
/// ~80 B, so the bucket array is ~40% denser/smaller (fewer cache misses on
/// a large keyspace, less RSS). The extra pointer-chase lands only on
/// collection ops, not the hot string GET path.
///
/// `Arc` (same 8 B as the previous `Box`) is what makes O(short-pause)
/// persistence possible: [`crate::Store::collect_snapshot`] bumps each
/// collection's refcount instead of serializing it, and a background thread
/// walks the frozen payloads at leisure. Mutations go through
/// [`std::sync::Arc::make_mut`] — a single uniqueness check (the steady
/// state, no snapshot in flight) or a copy-on-write deep clone when a
/// snapshot still holds the payload.
///
/// `Str` holds a [`SmallBytes`] (24 B, same size as `Vec<u8>`) so byte strings
/// up to 22 bytes live **inline inside the bucket**, killing the second cache
/// miss the value pointer-chase used to cost on large-keyspace GETs.
/// `Clone` is the snapshot-collect primitive: `Str` copies its bytes
/// (inline = 24 B memcpy; heap = one allocation), collections bump a
/// refcount. See [`crate::Store::collect_snapshot`].
///
/// ```
/// use kevy_store::{SetCondition, Store, Value};
/// let mut s = Store::new();
/// s.set(b"k", b"v".to_vec(), None, SetCondition::Always);
/// s.rpush(b"l", &[b"a".as_slice()])?;
/// let mut types = Vec::new();
/// s.snapshot_each(|_, v: &Value, _| types.push(v.type_name()));
/// types.sort();
/// assert_eq!(types, ["list", "string"]);
/// # Ok::<(), kevy_store::StoreError>(())
/// ```
#[derive(Debug, Clone)]
pub enum Value {
    /// A byte string, inline up to 22 bytes — see the type doc above for
    /// why that boundary is where it is.
    ///
    /// ```
    /// use kevy_store::{SetCondition, Store, Value};
    /// let mut s = Store::new();
    /// s.set(b"k", b"hello".to_vec(), None, SetCondition::Always);
    /// let mut hit = false;
    /// s.snapshot_each(|_, v, _| hit |= matches!(v, Value::Str(b) if b.as_ref() == b"hello"));
    /// assert!(hit);
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    Str(SmallBytes),
    /// Following valkey's OBJ_ENCODING_INT: when a SET
    /// stores a clean canonical i64 ASCII string (parses round-trip), we
    /// keep the integer **as i64** rather than as 22 B of inline bytes.
    /// Wins on INCR (in-place `+= delta`, no parse / no format / no
    /// SmallBytes wrap) and on memory (8 B vs 24 B). GET formats it via
    /// a per-`Store` scratch buffer.
    ///
    /// ```
    /// use kevy_store::{SetCondition, Store, Value};
    /// let mut s = Store::new();
    /// s.set(b"n", b"41".to_vec(), None, SetCondition::Always);
    /// assert_eq!(s.incr_by(b"n", 1)?, 42);
    /// let mut hit = false;
    /// s.snapshot_each(|_, v, _| hit |= matches!(v, Value::Int(42)));
    /// assert!(hit);
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    Int(i64),
    /// Values larger
    /// than [`BULK_THRESHOLD`] bytes get stored behind an
    /// `Arc<Box<[u8]>>` instead of a heap-backed `SmallBytes`. The Arc
    /// lets the io_uring reactor's reply path borrow the bytes across
    /// the SQE→CQE window safely (Arc clone keeps them alive even if
    /// the keyspace mutates) — the prerequisite for the writev
    /// zero-copy bulk reply path, which skips the per-GET memcpy from
    /// value storage into the per-conn output buffer.
    ///
    /// **Why `Arc<Box<[u8]>>` and not `Arc<[u8]>`**: `Arc<[u8]>` is a
    /// DST-backed `ArcInner<[u8]> = { strong, weak, [u8; N] }` whose
    /// data slot sits past the refcount words. `Arc::from(Vec<u8>)`
    /// allocates a fresh `ArcInner` and `copy_from_slice`s the bytes
    /// — a hard mandatory 64 KiB memcpy on every big SET. With
    /// `Arc<Box<[u8]>>`, the `Box<[u8]>` wrapper occupies the Arc's
    /// data slot (16 B), pointing AT an unchanged heap buffer; so
    /// `Arc::new(vec.into_boxed_slice())` is **truly zero-copy**
    /// (the boxed slice's allocation stays put — only the 32-byte
    /// `ArcInner` is freshly malloced). Per-GET cost: one extra
    /// pointer dereference (`&**arc` to get `&[u8]`), measured to be
    /// negligible vs the per-SET memcpy savings. The `Arc<[u8]>`
    /// mandatory copy was confirmed with perf-record before switching
    /// to `Arc<Box<[u8]>>`.
    ///
    /// Small values stay on `Str(SmallBytes)` because the inline
    /// cache-line storage beats an Arc indirection for the common case.
    ///
    /// ```
    /// use kevy_store::{SetCondition, Store, Value};
    /// let mut s = Store::new();
    /// s.set(b"k", vec![b'x'; 1024], None, SetCondition::Always);
    /// let mut hit = false;
    /// s.snapshot_each(|_, v, _| hit |= matches!(v, Value::ArcBulk(b) if b.len() == 1024));
    /// assert!(hit);
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    ArcBulk(Arc<Box<[u8]>>),
    /// A hash below [`HS_PROMOTE`](crate::seg_map::HS_PROMOTE) elements: one map behind one `Arc`, so
    /// a snapshot pins it whole and the first write during that window
    /// deep-clones it. Past that size it becomes `SegHash`.
    ///
    /// ```
    /// use kevy_store::{SetCondition, Store, Value};
    /// let mut s = Store::new();
    /// let items: Vec<Vec<u8>> = (0..10).map(|i| format!("field-{i}").into_bytes()).collect();
    /// let refs: Vec<&[u8]> = items.iter().map(|m| m.as_slice()).collect();
    /// let pairs: Vec<(&[u8], &[u8])> = refs.iter().map(|f| (*f, &b"v"[..])).collect();
    /// s.hset(b"h", &pairs)?;
    /// let mut hit = false;
    /// s.snapshot_each(|_, v, _| hit |= matches!(v, Value::Hash(h) if h.len() == 10));
    /// assert!(hit);
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    Hash(Arc<HashData>),
    /// A hash past `seg_map::HS_PROMOTE` fields: an extendible-hash
    /// directory of `Arc`-shared buckets — a COW write under a live
    /// snapshot view clones one bucket, not the whole value.
    ///
    /// ```
    /// use kevy_store::{SetCondition, Store, Value};
    /// let mut s = Store::new();
    /// let many: Vec<Vec<u8>> = (0..16 * 1024 + 1).map(|i| format!("m{i}").into_bytes()).collect();
    /// let refs: Vec<&[u8]> = many.iter().map(|m| m.as_slice()).collect();
    /// let pairs: Vec<(&[u8], &[u8])> = refs.iter().map(|m| (*m, *m)).collect();
    /// s.hset(b"h", &pairs)?;
    /// let mut hit = false;
    /// s.snapshot_each(|_, v, _| hit |= matches!(v, Value::SegHash(_)));
    /// assert!(hit);
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    SegHash(Arc<crate::seg_map::SegMap<SmallBytes>>),
    /// A list below [`SEG_PROMOTE`](crate::list_seg::SEG_PROMOTE) elements: one deque behind one `Arc`,
    /// with the same whole-value copy-on-write. Past that size it becomes
    /// `SegList`.
    ///
    /// ```
    /// use kevy_store::{SetCondition, Store, Value};
    /// let mut s = Store::new();
    /// let items: Vec<Vec<u8>> = (0..10).map(|i| format!("item-{i}").into_bytes()).collect();
    /// let refs: Vec<&[u8]> = items.iter().map(|m| m.as_slice()).collect();
    /// s.rpush(b"l", &refs)?;
    /// let mut hit = false;
    /// s.snapshot_each(|_, v, _| hit |= matches!(v, Value::List(l) if l.len() == 10));
    /// assert!(hit);
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    List(Arc<ListData>),
    /// A list past [`crate::list_seg::SEG_PROMOTE`] elements: a deque of
    /// `Arc`-shared segments so a COW write under a live snapshot view
    /// clones one segment, not the whole (possibly multi-GB) value. See
    /// `list_seg.rs` for the promotion contract.
    ///
    /// ```
    /// use kevy_store::{SetCondition, Store, Value};
    /// let mut s = Store::new();
    /// let many: Vec<Vec<u8>> = (0..16 * 1024 + 1).map(|i| format!("m{i}").into_bytes()).collect();
    /// let refs: Vec<&[u8]> = many.iter().map(|m| m.as_slice()).collect();
    /// s.rpush(b"l", &refs)?;
    /// let mut hit = false;
    /// s.snapshot_each(|_, v, _| hit |= matches!(v, Value::SegList(_)));
    /// assert!(hit);
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    SegList(Arc<crate::list_seg::SegListData>),
    /// A set below [`HS_PROMOTE`](crate::seg_map::HS_PROMOTE) elements, on the same terms as `Hash`.
    ///
    /// ```
    /// use kevy_store::{SetCondition, Store, Value};
    /// let mut s = Store::new();
    /// let items: Vec<Vec<u8>> = (0..10).map(|i| format!("member-{i}").into_bytes()).collect();
    /// let refs: Vec<&[u8]> = items.iter().map(|m| m.as_slice()).collect();
    /// s.sadd(b"s", &refs)?;
    /// let mut hit = false;
    /// s.snapshot_each(|_, v, _| hit |= matches!(v, Value::Set(m) if m.len() == 10));
    /// assert!(hit);
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    Set(Arc<SetData>),
    /// A set past `seg_map::HS_PROMOTE` members — the set door of the
    /// same bucket-sharded COW as [`Value::SegHash`].
    ///
    /// ```
    /// use kevy_store::{SetCondition, Store, Value};
    /// let mut s = Store::new();
    /// let many: Vec<Vec<u8>> = (0..16 * 1024 + 1).map(|i| format!("m{i}").into_bytes()).collect();
    /// let refs: Vec<&[u8]> = many.iter().map(|m| m.as_slice()).collect();
    /// s.sadd(b"s", &refs)?;
    /// let mut hit = false;
    /// s.snapshot_each(|_, v, _| hit |= matches!(v, Value::SegSet(_)));
    /// assert!(hit);
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    SegSet(Arc<crate::seg_map::SegMap<()>>),
    /// A sorted set: members with scores, plus the order-statistic tree
    /// that makes rank queries a lookup rather than a scan.
    ///
    /// ```
    /// use kevy_store::{SetCondition, Store, Value};
    /// let mut s = Store::new();
    /// s.zadd(b"z", &[(1.0, b"a".as_slice()), (2.0, b"b"), (3.0, b"c"), (4.0, b"d"), (5.0, b"e")])?;
    /// let mut hit = false;
    /// s.snapshot_each(|_, v, _| hit |= matches!(v, Value::ZSet(_)));
    /// assert!(hit);
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    ZSet(Arc<ZSetData>),
    /// A zset past `zset_seg::Z_PROMOTE` members — sharded member map
    /// + ordered segments; COW writes clone one bucket + one segment.
    ///
    /// ```
    /// use kevy_store::{SetCondition, Store, Value};
    /// let mut s = Store::new();
    /// let many: Vec<Vec<u8>> = (0..16 * 1024 + 1).map(|i| format!("m{i}").into_bytes()).collect();
    /// let refs: Vec<&[u8]> = many.iter().map(|m| m.as_slice()).collect();
    /// let pairs: Vec<(f64, &[u8])> = refs.iter().enumerate().map(|(i, m)| (i as f64, *m)).collect();
    /// s.zadd(b"z", &pairs)?;
    /// let mut hit = false;
    /// s.snapshot_each(|_, v, _| hit |= matches!(v, Value::SegZSet(_)));
    /// assert!(hit);
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    SegZSet(Arc<crate::zset_seg::SegZSetData>),
    /// A stream: entries, consumer groups and their pending lists. Never
    /// segmented — a stream trims from the front instead of growing
    /// without bound.
    ///
    /// ```
    /// use kevy_store::{SetCondition, Store, Value};
    /// let mut s = Store::new();
    /// use kevy_store::{MissingStream, XAddIdSpec};
    /// let fields = vec![(b"temp".to_vec(), b"21".to_vec())];
    /// s.xadd(b"st", XAddIdSpec::AutoAll, fields, MissingStream::Create, 1_000)?;
    /// let mut hit = false;
    /// s.snapshot_each(|_, v, _| hit |= matches!(v, Value::Stream(_)));
    /// assert!(hit);
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    Stream(Arc<crate::stream::StreamData>),
    /// Valkey-orthodox encoding switch: tiny sets (1-N
    /// short members) live inline in 24 bytes instead of behind
    /// `Arc<SetData>` — matches valkey's `OBJ_ENCODING_LISTPACK` for
    /// sets, which is what `redis-benchmark -t sadd` default `-r 0`
    /// (cardinality stays at 1 forever, single 20-byte literal member)
    /// measures. On overflow (the inline form has no room for another
    /// member) the set is promoted to `Value::Set(Arc<SetData>)`
    /// — the Swiss-table path that wins for larger cardinalities.
    ///
    /// ```
    /// use kevy_store::{SetCondition, Store, Value};
    /// let mut s = Store::new();
    /// s.sadd(b"s", &[b"only".as_slice()])?;
    /// let mut hit = false;
    /// s.snapshot_each(|_, v, _| hit |= matches!(v, Value::SmallSetInline(_)));
    /// assert!(hit);
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    SmallSetInline(crate::small_set::SmallSetData),
    /// Tiny hashes
    /// (1-2 short field-value pairs) live inline in 24 bytes; promoted
    /// to `Value::Hash(Arc<HashData>)` on overflow. Mirrors valkey's
    /// `OBJ_ENCODING_LISTPACK` for hashes.
    ///
    /// ```
    /// use kevy_store::{SetCondition, Store, Value};
    /// let mut s = Store::new();
    /// s.hset(b"h", &[(b"f".as_slice(), b"v".as_slice())])?;
    /// let mut hit = false;
    /// s.snapshot_each(|_, v, _| hit |= matches!(v, Value::SmallHashInline(_)));
    /// assert!(hit);
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    SmallHashInline(crate::small_hash::SmallHashData),
    /// A declared table's row: the columns in declared order, in one payload
    /// buffer, with no field names and no per-row table.
    ///
    /// Reachable only for a key under a declared prefix — an undeclared hash
    /// keeps [`Value::Hash`].
    ///
    /// ```
    /// use kevy_store::{SetCondition, Store, Value};
    /// let mut s = Store::new();
    /// s.hset(b"user:1", &[(b"id".as_slice(), b"7".as_slice())])?;
    /// s.pack_row(b"user:1", &vec![b"id".to_vec(), b"name".to_vec()].into());
    /// let mut hit = false;
    /// s.snapshot_each(|_, v, _| hit |= matches!(v, Value::PackedRow(r) if r.get_named(b"id") == Some(&b"7"[..])));
    /// assert!(hit);
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    PackedRow(crate::packed_row::PackedRow),
    /// Tiny lists inline encoding; promoted to
    /// `Value::List(Arc<ListData>)` on overflow.
    ///
    /// ```
    /// use kevy_store::{SetCondition, Store, Value};
    /// let mut s = Store::new();
    /// s.rpush(b"l", &[b"a".as_slice(), b"b"])?;
    /// let mut hit = false;
    /// s.snapshot_each(|_, v, _| hit |= matches!(v, Value::SmallListInline(_)));
    /// assert!(hit);
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    SmallListInline(crate::small_list::SmallListData),
    /// Tiny sorted sets inline encoding; promoted to
    /// `Value::ZSet(Arc<ZSetData>)` on overflow.
    ///
    /// ```
    /// use kevy_store::{SetCondition, Store, Value};
    /// let mut s = Store::new();
    /// s.zadd(b"z", &[(1.0, b"a".as_slice())])?;
    /// let mut hit = false;
    /// s.snapshot_each(|_, v, _| hit |= matches!(v, Value::SmallZSetInline(_)));
    /// assert!(hit);
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    SmallZSetInline(crate::small_zset::SmallZSetData),
    /// A demoted (tiered-to-disk) value's in-map stub. The two-stage
    /// funnel (`tier` module) resolves this before any typed match sees
    /// it: stage 1 answers existence/TYPE/TTL from the stub with zero
    /// IO; stage 2 materializes (serve or promote) only on a type
    /// match. Cloning a `Cold` clones the STUB, not the record — paths
    /// that duplicate values (COPY, cross-shard ship) materialize
    /// first so two stubs never alias one vlog record.
    ///
    /// ```
    /// use kevy_store::{SetCondition, Store, Value};
    /// # let dir = std::env::temp_dir().join(format!("kevy-doc-value-cold-{}", std::process::id()));
    /// let mut s = Store::new();
    /// s.enable_tiering(&dir, 1 << 20)?;
    /// s.set(b"k", vec![b'x'; 4096], None, SetCondition::Always);
    /// s.set_tier_budget(1);
    /// s.demote_to_watermark();
    /// let mut cold = false;
    /// s.snapshot_each(|_, v, _| cold |= matches!(v, Value::Cold(_)));
    /// assert!(cold);
    /// // the stub still answers TYPE without touching disk
    /// assert_eq!(s.type_of(b"k"), "string");
    /// # std::fs::remove_dir_all(&dir)?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    Cold(ColdRef),
}

/// Threshold (bytes) above which a SET stores its value as
/// [`Value::ArcBulk`] (writev-eligible on GET) instead of [`Value::Str`]
/// (inline `SmallBytes`). 64 B ≈ one cache line — below that the
/// inline-SmallBytes storage wins on L1 locality; above it the
/// writev-borrow win dominates.
///
/// ```
/// use kevy_store::{BULK_THRESHOLD, SetCondition, Store, Value};
/// let mut s = Store::new();
/// s.set(b"at", vec![b'x'; BULK_THRESHOLD], None, SetCondition::Always);
/// s.set(b"over", vec![b'x'; BULK_THRESHOLD + 1], None, SetCondition::Always);
/// let mut kinds = Vec::new();
/// s.snapshot_each(|k, v, _| kinds.push((k.to_vec(), matches!(v, Value::ArcBulk(_)))));
/// kinds.sort();
/// assert_eq!(kinds, [(b"at".to_vec(), false), (b"over".to_vec(), true)]);
/// ```
pub const BULK_THRESHOLD: usize = 64;

const _: () = {
    // Don't let future variants undo box-collection's Entry-48B win.
    assert!(core::mem::size_of::<Value>() <= 32);
};

/// Heap-size threshold above which an overwritten `Value` is sent to the
/// runtime's bio thread for off-reactor drop instead of being freed inline
/// (lazy-drop).
///
/// **Why not lower**: a 256 B threshold regressed c=50 -d 10240 SET
/// p999 from 0.487 → 1.583 ms (worse by 3.25×). The cause: `std::sync::mpsc::Sender::send`
/// is a few hundred ns of atomic + Box clone, which EXCEEDS the inline
/// `Box::<[u8]>::drop` cost when the allocator serves the free from a
/// hot large-class slab (~ 1-3 µs for 10 KB; the bench's steady state).
/// Off-loading only wins when the inline drop's tail risk (cold-slab
/// `munmap`/`madvise` consolidation stall, observed at 50-150 µs and
/// occasionally millisecond-range) exceeds the per-send channel cost
/// PLUS the cross-thread cache-line bouncing.
///
/// With per-shard batch accumulation flushing at the end of every
/// reactor iteration, the per-mpsc-send cost is amortised across N
/// drops. That makes the channel hop profitable at smaller sizes than
/// a lone-send model could justify (lone-send had to lift the
/// threshold to 16 KB because per-`mpsc::send` cost was a few hundred
/// ns — at 256 B the inline drop was cheaper).
///
/// **Sweet-spot surprise**: intuition suggested dropping the threshold
/// to 256 B – 1 KB once batching amortises the send. A sweep across
/// thresholds {512, 1024, 4096, 16384} × c=50 SET -d {1K, 4K, 10K, 64K}
/// disproved that floor: at ≤ 1 KB threshold, p999 / max on small
/// values (-d 1024, -d 4096) was variance-bounded equal or
/// occasionally WORSE than a 16 KB threshold, while the larger
/// sizes (10 KB / 64 KB) won either way. Cause: the Vec::push +
/// occasional `MAX_PENDING_DROPS` force-flush stall costs more for
/// small Arcs (allocator small-class free is sub-µs even at tail)
/// than the inline drop it avoids.
///
/// Picked **4 KB** as the lowest threshold where the bio-off-reactor
/// win consistently dominates the batch-buffer overhead on tail
/// metrics. The biggest batching wins (vs lone-send at 16 KB) land on
/// `-d 64K` SET p50 (-44 %) and `-d 10K` SET max (-35 %), where each
/// iter's batch already contains several heavy values per shard.
///
/// ```
/// use kevy_store::{HEAP_HEAVY_BYTES, SetCondition, Store};
/// let (tx, rx) = std::sync::mpsc::channel();
/// let mut s = Store::new();
/// s.set_bio_drop_sender(tx);
/// s.set(b"light", vec![b'x'; HEAP_HEAVY_BYTES - 1], None, SetCondition::Always);
/// s.set(b"heavy", vec![b'x'; HEAP_HEAVY_BYTES], None, SetCondition::Always);
/// // overwrite both; only the heavy value is shipped off for dropping
/// s.set(b"light", b"1".to_vec(), None, SetCondition::Always);
/// s.set(b"heavy", b"1".to_vec(), None, SetCondition::Always);
/// s.flush_pending_drops();
/// assert_eq!(rx.try_recv()?.len(), 1);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub const HEAP_HEAVY_BYTES: usize = 4 * 1024;

/// Sender half of the runtime's bio-drop channel. Wired from
/// `kevy-rt`'s `bio.rs` via [`crate::Store::set_bio_drop_sender`]; the
/// concrete payload is `Vec<Value>` — a **batch** of values
/// produced by one shard since its last flush.
/// The bio thread (`kevy-rt::bio::spawn`) iterates the batch and
/// drops each item. One mpsc message per shard-flush amortises the
/// channel cost (atomic + cross-thread cacheline traffic) across
/// however many values landed in the batch.
///
/// ```
/// use kevy_store::{BioDropSender, SetCondition, Store};
/// let (tx, rx) = std::sync::mpsc::channel();
/// let sender: BioDropSender = tx;
/// let mut s = Store::new();
/// s.set_bio_drop_sender(sender);
/// s.set(b"k", vec![b'x'; 64 * 1024], None, SetCondition::Always);
/// s.set(b"k", b"small".to_vec(), None, SetCondition::Always);
/// s.flush_pending_drops();
/// // the displaced 64 KiB value arrives in one batch for the drop thread
/// let batch = rx.try_recv()?;
/// assert_eq!(batch[0].type_name(), "string");
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[cfg(feature = "std")]
pub type BioDropSender = std::sync::mpsc::Sender<Vec<Value>>;

impl Value {
    /// The Redis type name (`TYPE` command).
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Str(_) | Value::Int(_) | Value::ArcBulk(_) => "string",
            Value::Hash(_)
            | Value::SegHash(_)
            | Value::SmallHashInline(_)
            | Value::PackedRow(_) => "hash",
            Value::List(_) | Value::SegList(_) | Value::SmallListInline(_) => "list",
            Value::Set(_) | Value::SegSet(_) | Value::SmallSetInline(_) => "set",
            Value::ZSet(_) | Value::SegZSet(_) | Value::SmallZSetInline(_) => "zset",
            Value::Stream(_) => "stream",
            // Stage-1 funnel: TYPE (and SCAN's TYPE filter) answer from
            // the tag — a cold key never pays a pread for its type.
            Value::Cold(c) => c.type_name(),
        }
    }
}

// `BioDropSender = mpsc::Sender<Box<Value>>` requires `Value: Send`. Static
// assert: if a future variant inadvertently makes Value `!Send` (e.g. an
// `Rc<...>` payload) this fails at compile time, BEFORE the runtime tries
// to hand a value to the bio thread.
const _: fn() = || {
    fn assert_send<T: Send>() {}
    assert_send::<Value>();
};
