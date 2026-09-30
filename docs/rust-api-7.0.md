# Rust API changes in 7.0

7.0 brings every public Rust API of the workspace in line with the
[Rust API Guidelines](https://rust-lang.github.io/api-guidelines/). This
page lists every change a caller can see, crate by crate, as
old → new. A program that talks to kevy over the wire needs none of it;
the [upgrade guide](upgrading-6.4-to-7.0.md) covers what else 7.0 changes.

## The patterns

Most of the changes are one of six shapes, applied everywhere they fit:

- **A `bool` parameter is an enum named for its meaning.**
  `store.copy(a, b, true)` is `store.copy(a, b, CopyMode::Replace)`. Where
  two flags could combine illegally, one enum takes both
  (`set(k, v, e, nx, xx)` → `SetCondition::{Always, IfAbsent, IfPresent}`).
  A setter named for its flag, with the flag as its only argument, keeps
  its `bool`.
- **Structs and enums the library may grow are `#[non_exhaustive]`.** A
  struct you build takes `Default` and assignment or its `with_*`
  builders, or `new(…)` where a field has no sensible default:

  ```rust
  // 6.4
  let p = HnswParams { m: 16, ef_construction: 200, distance: Distance::Cosine };
  // 7.0
  let p = HnswParams::default().with_m(16).with_ef_construction(200)
      .with_distance(Distance::Cosine);
  ```

  A `match` on an enum from another crate needs a `_` arm.
- **A type with invariants keeps its fields private** and exposes them as
  methods of the same name without a `get_` prefix (`civil.y` →
  `civil.year()`).
- **A free function that took a type first is a method on it**
  (`epoch_from_civil(c)` → `c.to_epoch()`, `parse_url(u)` →
  `ParsedUrl::parse(u)`).
- **An error is a type, not a `String`.** Each implements `Display` and
  `std::error::Error`, is `Send + Sync`, and where the text reached the
  wire, `as_wire()` or `to_wire()` gives the same text as 6.4.
- **Traits not meant for outside implementation are sealed.** The ones
  that stay open — `kevy_rt::Commands`, `kevy_client_async::AsyncTransport`,
  `kevy_resp::ArgvView` — document what an implementation must uphold.

Every public type now implements `Debug` (key material is never printed),
and every public item carries a running example in its documentation.

## kevy-embedded

- `copy(src, dst, bool)` → `copy(src, dst, CopyMode)`: `IfAbsent`
  (default) or `Replace`.
- `hrandfield(key, n, with_values)` → `hrandfield(key, n)` and
  `hrandfield_with_values(key, n)`.
- `linsert(key, before, pivot, value)` → `linsert(key, InsertPosition,
  pivot, value)`: `Before` or `After`.
- `view_create(name, tree, order_by, desc, mode)` → `view_create(name,
  tree, order_by, SortOrder, mode)`.
- `idx_create_text(name, prefix, fields, positions: bool, values)` →
  `TokenPositions::{Omit (default), Record}` in that place.
- `Config::with_replay_resync(bool)` and the field `replay_resync` →
  `with_replay_mode(ReplayMode)` and `replay_mode` (`Strict` or `Resync`).
- `ScalarQueryOpts` and `MatchOpts`: `sort` is `(field, SortOrder)`; both
  are `#[non_exhaustive]` with `with_filters`, `with_sort`,
  `with_distinct`, `with_facets`, `with_offset`, … builders.
- The change feed takes and returns a `FeedPosition { generation, offset }`
  where it used two integers: `changes_tail() -> FeedPosition`,
  `changes_since(FeedPosition, limit, prefixes)`, `ChangeBatch::next`,
  `FeedError::Resync { tail: FeedPosition }`. `FeedError` implements
  `Display` and `Error`.
- `PubsubFrame` → `PubsubEvent` (the subscriber count is `i64`).
- `TierBudgetSpec` is `kevy_config::TierBudgetSpec`; the `tier` feature
  pulls in kevy-config.
- `LinkKeys::new(local).with_peers(peers)`.
- `keys_iter(pattern)` returns `KeysIter`, which walks the keyspace a
  page at a time, instead of `std::vec::IntoIter` over a copy of every
  key. `scan(cursor, pattern, count)` has the server's contract: the
  cursor names a shard and a position in it, `count` is how much a page
  walks (a page may hold more), and a key written during the walk may come
  back twice. 6.4 copied every key on each call and sliced the copy.
- `idx_drop`, `view_drop` and `table_drop` return `KevyResult<bool>`.
  Every catalog method (`idx_create*`, `view_create`, `table_declare`,
  `table_ensure`, `table_replace` and the drops) is refused on a replica
  with `KevyError::ReadOnly` and after `shutdown` with `KevyError::Closed`,
  like every write.
- `AtomicAllShards::idx_query` / `idx_count` see the transaction's own
  writes (6.4 saw the last commit); a failed transaction's writes leave
  the index with its rollback.
- New: `Change::new(offset, argv)`, `ChangeBatch::new(changes, next)`, and
  re-exports of `SortOrder`, `InsertPosition`, `ReplayMode`,
  `FeedPosition`, `CopyMode`, `TokenPositions`, `PubsubEvent`.
- `#[non_exhaustive]`: `Config`, `TtlReaperMode`, `ValueFilter`,
  `KevyInfo`, `KevyTierInfo`, `IdxAdvice`, `ScalarPage`, `MatchPage`,
  `ReconcileReport`, `SnapshotEntry`, `Change`, `ChangeBatch`,
  `PrefixInfo`.

```rust
use kevy_embedded::{CopyMode, FeedPosition, InsertPosition, ScalarQueryOpts, SortOrder};

store.copy(b"src", b"dst", CopyMode::Replace)?;
store.linsert(b"l", InsertPosition::Before, b"c", b"b")?;
let opts = ScalarQueryOpts::default().with_sort(b"price", SortOrder::Desc);
let from: FeedPosition = store.changes_tail()?;
let batch = store.changes_since(from, 100, &[])?;
```

## kevy-client and kevy-client-async

- kevy-client's `FeedFrame { offset, argv }` → `Change` (the same type as
  `kevy_embedded::Change`); `FeedBatch { generation, next_offset, frames }`
  → `ChangeBatch { next: FeedPosition, changes }`.
- `feed_tail(shard) -> FeedPosition`; `feed_read(shard, from:
  FeedPosition, count, prefixes)`.
- `#[non_exhaustive]`: `Connection`, `IdxType`, `IdxInfo`, `IdxPage`,
  `IdxRow`.
- kevy-client-async: `AsyncConnection::from_transport<T: AsyncTransport>(T)`;
  `Pipeline::run<T>(&mut AsyncConnection<T>)`.
- kevy-cluster-rw: `request_read(args, bool)` → `request_read(args,
  ReadConsistency)`: `Eventual` (default) or `Primary`.

## kevy-config

- Every section struct, `Config` and `CliOverrides` are
  `#[non_exhaustive]`: start from `Default` and assign.
  `PeerEntry::new(id, host, port).with_client_port(..).with_repl_port_base(..)`
  and `ScopeEntry::new(prefix, writer).with_fallback(..)` have no default.
- `NotificationFlags` is a bitset: `KEYSPACE | KEYEVENT | …`, `contains`.
  `is_empty()` now means no bit is set; 6.4's meaning ("nothing will be
  published") is `!is_active()`. `parse_notification_flags(s)` →
  `s.parse::<NotificationFlags>()`.
- `parse_size`, `key_from_hex`, `TierBudgetSpec::parse`,
  `PeerEntry::parse_list` and `ScopeEntry::parse_list` return `ValueError`
  where they returned a `String`; `parse_list`'s text is now
  `bad peer token: "x"`.
- `LogOutput::as_str` → `to_config_str`;
  `TierBudgetSpec::as_config_string` → `to_config_string`.
- `EvictionPolicy` is `kevy_store::EvictionPolicy`; `AppendFsync` is
  `kevy_persist::Fsync`.
- `AuditSection`, `FeedSection` and `MetricsSection` are exported at the
  crate root. Enums and `ConfigError` are `#[non_exhaustive]`.
- The new fields for encrypted links and the proxy-friendly cluster are in
  the [upgrade guide](upgrading-6.4-to-7.0.md#10-kevy-config-new-fields-on-the-section-structs).

## kevy-store

Flags that were `bool`s:

| 6.4 | 7.0 |
|---|---|
| `set(k, v, e, nx, xx)`, `set_slice(…)` | `SetCondition::{Always, IfAbsent, IfPresent}` |
| `lmove(s, d, from_left, to_left)` | `ListEnd` for each end |
| `linsert(k, before, p, v)` | `InsertPosition::{Before, After}` |
| `rename(s, d, nx)` | `rename(s, d)` and `rename_nx(s, d)` |
| `hrandfield(k, n, false)` / `(k, n, true)` | `hrandfield(k, n)` / `hrandfield_with_values(k, n)` |
| `set_notify_capture(n, x, e)` | `set_notify_capture(impl IntoIterator<Item = KeyspaceEvent>)` |
| `xadd(…, nomkstream, now)`, `xgroup_create(…, mkstream)` | `MissingStream::{Create, Refuse}` |
| `xreadgroup(…, noack)` | `AckMode::{Pending, NoAck}` |
| `xautoclaim(…, justid)` | `ClaimMode::{Deliver, JustId}` |
| `parse_explicit_id(s, end)` | `parse_explicit_id(s)` |

Types:

- `XClaimOpts::default().with_min_idle_ms()`, `with_idle_ms`,
  `with_time_ms`, `with_retrycount`, `with_force`, `with_mode(ClaimMode)`
  (the field `justid` is `mode`).
- `ZaddFlags::new(SetCondition, ScoreCompare::{Any, Greater, Less})`
  returns `Option` (`None` for an illegal combination) and takes
  `with_ch`; read it with `condition()`, `compare()`, `ch()`. `valid()` is
  gone.
- `StreamId::new(ms, seq)`, `LoadedGroup::new(..)`,
  `ScoreBound::inclusive(v)` / `exclusive(v)`, `Score::new(x)` /
  `.value()`.
- `ConsumerGroup`'s fields are methods: `last_delivered_id()`,
  `pending_entry(id)`, `pending_range(r)`, `consumer(name)`,
  `consumers()`. `ConsumerState`: `name()`, `pending_count()`,
  `last_seen_ms()`. `SealedRows`: `seq()`, `file()`.
- Renamed: `iter_entries` / `groups_iter` → `entries()` / `groups()`;
  `SegListData::iter_range` → `range()`; `SmallSetData::iter_slices` is
  gone; `bitop_combine(op, s, n)` → `op.combine(s, n)`;
  `apply_segmented(store, dir, f)` → `store.apply_segmented(dir, f)`.
- Errors: the segment row functions return `SegRowsError`; `StoreError`
  and `StreamIdError` implement `Display` and `Error`;
  `StoreError::as_wire()` gives the reply text. `KevyError::ReadOnly`
  displays `write refused: this is a read-only replica`;
  `KevyError::Store(e)` displays the store error's text and returns it
  from `source()`.
- New: `Store::set_row_watch(RowWatch)`, `take_row_changes`,
  `has_row_changes`, `row_watch`, with `RowWatch`, `RowChanges`,
  `RowChange`: the store records, for keys under a watched prefix, the
  watched fields as they were before the first write since the last
  take — what an index needs to drop a row's old entry, whichever path
  wrote the row.
- New: `KeyspaceEvent::name()`, `HExpireCond::keyword()`,
  `ZAggregate::keyword()`, `EvictionPolicy::as_str()` / `parse()`.
- `#[non_exhaustive]`: `KevyError`, `StoreError`, `StreamIdError`,
  `BitOp`, `HExpireCond`, `KeyspaceEvent`, `XAddIdSpec`, `GroupCreateMode`,
  `ReadGroupId`, `EvictionPolicy`, `ZAggregate`, `SetCondition`,
  `ScoreCompare`, `ExpireStats`, `TierStats`, `ColdRead`,
  `AutoclaimResult`, the `Pending*` replies, `PelEntry`, `ZaddReport`.

## kevy-index

- `IndexSpec { .. }` and `IndexSpec::single_field(..)` →
  `IndexSpec::builder(name, prefix, kind, ty).with_field(f)` and optionally
  `.with_fields`, `.with_max_bytes`, `.with_ann`, `.with_group_by`,
  `.with_positions`, `.with_values`, `.with_composite`, then `.build()?`
  (error `SpecError`). The fields are methods of the same name;
  `with_positions` reads as `has_positions()`. The field checks moved from
  `Catalog::create` to `build()`. `Catalog::coerce` →
  `IndexValue::coerce(spec.ty(), raw)`.
- Builders: `FieldSpec::new(n).with_weight(w)`,
  `ValueSpec::new(n).with_type(ty)`,
  `AnnSpec::new(dim).with_distance(d).with_m(m).with_ef(ef)`,
  `CompositeCol::new(n, ty).with_order(SortOrder)`,
  `OrderPath::new(name, Vec<(col, SortOrder)>)`,
  `ViewSpec::new(name, tree, order_by).with_order(..).with_mode(..).with_via(..)`,
  `ScalarClauses::new(fetch).with_filters(..).with_sort(pos, SortOrder, ty).with_distinct(..).with_facets(..)`.
  The fields named `desc` are `order: SortOrder`.
- Constructors: `Leaf::new`, `TableIndex::new`, `WindowSpec::new`,
  `Cursor::new`, `ScalarHit::new`, `AdviseEntry::new`, `IndexVerify::new`,
  `GlobalPath::new`, `WindowAudit::new`. `TableSpec`, `TableVerify`,
  `GroupStats`, `SegmentStats`, `AggStats` and `WhereClause` take
  `Default` and assignment.
- Sort direction is `SortOrder` (re-exported from kevy-text):
  `Segment::scan(after, desc)` → `scan(after, SortOrder)`;
  `scalar_sorted_order` → `sorted_order(a, b, SortOrder)`;
  `merge_claused(all, sort_desc, grouped, off, lim)` → `(all,
  Option<SortOrder>, off, lim)`; `MaterializedSet::new(k, desc)` →
  `(k, SortOrder)`; `page(after, limit, desc)` → `page(after, limit)`.
- `AggSegment::apply(k, Some((g, v)), false)` →
  `AggRow::Member { group, value }`; `(k, None, false)` →
  `AggRow::Removed`; `(k, None, true)` → `AggRow::Excluded`.
  `MaterializedSet::apply(k, member, order)` →
  `Membership::Member(order) | NonMember`.
- Fields that are methods: `order_excluded()`; `UsageCell`'s fields
  (plus `min_margin()`).
- Functions that are methods: `compile_table(&t)` → `t.compile()`;
  `apply_auto` → `t.apply_auto(&e)`; `advice_of` → `e.advice(&cat)`;
  `window_for` → `cat.window_for(n)`; `window_driver` →
  `cat.is_window_driver(n)`; `window_text_for` → `cat.is_windowed_text(s)`;
  `value_order_bytes(&v)` → `v.order_bytes()`; `window_value_of` →
  `v.window_value(shape)`; `eval_tree` → `t.eval(seg)`; `key_in_tree` →
  `t.contains(k, seg)`; `key_in_tree_vals` → `t.contains_values(f)`;
  `parse_split_point(&s, raw)` → `s.parse_split_point(raw)`;
  `split_point_text` → `s.split_point_text(enc)`;
  `merge_group(&mut a, &b)` → `a.merge(&b)`;
  `narrow_advice(spec, i64)` → `spec.narrow_advice(Option<i64>)`;
  `splits_from_sample` → `splits_from_weighted(points, parts)`.
- A `Segment` keeps no map from key back to entry, so a write names the
  value the row was indexed under: `apply(key, new)` →
  `apply(key, old: Option<&IndexValue>, new)`;
  `apply_with_values(key, new, vals)` → `(key, old, new, vals)`;
  `remove(key)` → `remove(key, old: &IndexValue)`;
  `verify_entry(key)` → `contains(&value, key)`;
  `stored(key, field)` → `stored(&value, key, field) -> Option<Vec<u8>>`;
  `stored_row(key)` → `stored_row(&value, key) -> Vec<Option<Vec<u8>>>`;
  `max_value()` returns `Option<IndexValue>`.
- `scan(after, order)` returns a `Scan` cursor instead of a boxed
  iterator: `while let Some((value, key)) = scan.next_entry() { … }`;
  `iter_below(bound)` → `scan_below(bound)`, with `Scan::stored_row()`
  for the entry's `VALUES`.
- New: `Segment::for_spec(&IndexSpec)` (the segment an index declares,
  with its key prefix and value shape), `repack()`, `set_key_dir(bool)` /
  `key_dir() -> Option<&KeyDir>` (key → value, for indexes a view reads
  by key), `IndexSpec::derive_scalar_refs`. `PlacementTable` is gone.
  `each_entry` visits entries in `(value, key)` order; `count` is
  O(log n).
- Errors: `CatalogError` (`to_wire()`), `ViewError`, `TableError`
  (`to_wire()`), `WhereError`.
- New: `IndexValue::encode` / `decode` / `render`, `AggBy::tag`,
  `GroupStats::rank_score`, `ViewMode::name`.
- `#[non_exhaustive]`: `ValType`, `IndexKind`, `IndexState`,
  `AdviseShape`, `AggBy`, `Partitioning`, `WindowShape`, `TableEnsure`,
  `IndexValue`, `ViewMode`.

## kevy-text, kevy-vector, kevy-sql, kevy-window

- kevy-text: `sorted_order(a, b, bool)` → `sorted_order(a, b, SortOrder)`;
  `Sort { field, desc, key }` → `Sort::new(field, key).with_order(SortOrder::Desc)`;
  `Filter`, `Distinct`, `Facet`, `TextMatch`, `CorpusStats` → `::new(…)`;
  `QueryOpts` and `SegmentShape` → `::default().with_*`. `TextStats`,
  `FacetedMatches`, `FrozenBucket`, `FwdRecord`, `ColdEntry` are
  `#[non_exhaustive]`.
- kevy-vector: `HnswParams::default().with_m(..).with_ef_construction(..).with_distance(..)`;
  `HnswParams`, `VectorStats`, `Distance` are `#[non_exhaustive]`.
  `Hnsw::new` panics for `m < 2` (M = 1 gave unbounded levels), and an
  embedded `idx_create_ann` refuses M = 1 (M = 0 still means the default).
- kevy-sql: `KevyType` → `kevy_sql::ValType` (the same type as
  `kevy_index::ValType`); `Served::Yes { paths, view, card }` /
  `No { reason }` → `Served::View { paths, argv }` /
  `Card { paths, card }` / `Refused { reason }`, with `paths()`;
  `table_ddl` returns `DeclarationError`. `SqlError`, `CardParam`,
  `QueryCard`, `Compilation`, `Folded`, `Plan`, `PlanEntry`, `Served` are
  `#[non_exhaustive]`.
- kevy-window: `WindowRt`'s `spec`, `shape`, `idle_ticks` are methods;
  `ColdPageQuery { .. }` → `ColdPageQuery::parse(text, stats, fetch)` with
  `with_filter`, `with_sort`, `with_distinct`, `with_facets`; the cold
  functions return `ColdError`. `ColdHit`, `ColdPage`, `ColdPageQuery` are
  `#[non_exhaustive]`.

## kevy-persist and kevy-replicate

- kevy-persist: `Aof::open_with_repair(p, f, resync)` and
  `open_after_replay(p, f, resync, settled)` take `ReplayMode::{Strict,
  Resync}`; `replay_aof_in_place(p, resync, quiet, f)` →
  `(p, ReplayMode, ReplaySummary::{Print, Quiet}, f)`;
  `replay_aof_quiet(p, resync, f)` → `(p, ReplayMode, f)`.
- `Fsync` (still `kevy_persist::Fsync`) is `#[non_exhaustive]`, has
  `as_str` / `parse`, and defaults to `EverySec`.
- `RewritePolicy::default().with_pct(..).with_min_size(..).with_bytes(..).with_interval_secs(..)`;
  the default turns every rule off. `ShardsMeta::new(n, routing)`,
  `ShardsMeta::read(p)`, `m.write(p)`; `RewriteStats::default()`;
  `dump_aof` returns `RewriteStats`. Writers take `W: Write` by value
  (`&mut w` still compiles).
- Feed positions: `FeedBoot` / `FeedBoot::load` →
  `feed_meta::boot_position(dir, shard) -> io::Result<FeedPosition>`;
  `write_feed_meta(dir, shard, FeedPosition)`;
  `read_snapshot_cursor() -> Option<FeedPosition>`;
  `write_snapshot_to_with_cursor(src, sink, Option<FeedPosition>)`.
  `Routing` (default `KevyHash`) and `RewritePlan` are
  `#[non_exhaustive]`.
- A snapshot and a rewritten log can carry one frame beside the keyspace:
  `SnapshotSource::aux_frame` (default `None`), `WithAux::new(src, aux)`,
  `load_snapshot_with_aux`. `reshard::commit_reshard(dir, prev_n, target,
  stores, aux, layout)` takes the frame the new snapshots carry.
- kevy-replicate: `feed::FeedPosition { generation, offset }`
  (`#[non_exhaustive]`, `Copy`, `Default`, `const fn new`) replaces every
  (generation, offset) pair: `FeedSource::tail()`,
  `read(at: FeedPosition, max)`, `FeedRead::Resync { tail }`,
  `HandshakeReq { from, .. }`, `encode_ack`, `encode_ping`,
  `SnapshotMarker::Ping`, `ReplicaEvent::Ping`,
  `primary_at_handshake()`. `fresh_generation` is public.
- `connect_with_timeout`, `connect_at` and `connect_secure` →
  `ReplicaClient::connect_with(addr, &ConnectOptions::new(id).with_from(pos).with_timeout(t).with_security(sec))`;
  `connect(addr, id, offset)` stays. `ReplicaSecurity::new(local,
  primary_key)`. Encryption is the default feature `secure`; without it
  there is no `ReplicaSecurity` or security option.
- `handshake::parse_replicate_from(argv)` → `HandshakeReq::parse(argv)`;
  `wire::decode_frame` returns `(DecodedFrame, usize)`;
  `DecodedFrame::new(offset, argv)`. `ReplicaError` and `WireError`
  implement `source()`; `WireError`'s `PartialEq` compares whole values.
  `FeedFrame`, `HandshakeReq`, `DecodedFrame`, `ReplicaSlot`,
  `source::Frame`, `HandshakeError`, `ReplicaEvent`, `ReplicaError`,
  `WireError`, `SnapshotMarker` are `#[non_exhaustive]`.

## kevy-rt

- `NotifyClass` → `kevy_resp::NotifyKind`;
  `Commands::notify_class() -> Option<NotifyKind>`.
- `ScanArgs` → `Route::Scan(Result<ScanOpts, ScanOptsError>)`;
  `parse_slowlog_sub` → `SlowlogSub::parse`;
  `Route::ListMove { from: ListEnd, to: ListEnd }`;
  `ClientKillFilter::parse() -> Option<(Self, KillReply)>`.
- Builders: `ResolvedCmd::new(route).with_txn_kind(..).with_quit(..).with_write(..).with_block_hint(..).with_wake_idx(..)`;
  `XGroupCtx::new(group, consumer).with_ack(AckMode)`;
  `ReplicaAck::new(offset, age_ms)`;
  `ReplicationSecurity::new(local).with_replica_keys(keys)`;
  `LiveRuntimeConfig` takes `Default` and assignment.
- `Runtime::with_replication(enabled)` and
  `with_replication_buffer_size(bytes)`; `with_feed(enabled)` and
  `with_feed_buffer_size(bytes)`; `with_replay_resync(bool)` →
  `with_replay_mode(ReplayMode)`.
- `shard_of_key(key, n, bool)` → `shard_of_key(key, n, Routing)`.
- New `Commands` methods, with defaults: `snapshot_aux`,
  `load_snapshot_aux(frame, full_sync)` and `on_restored(record)`.
- `#[non_exhaustive]`: `Route`, `BlockHint`, `BlockKind`,
  `ClientKillFilter`, `GeoHits`, `SlowlogSub`, `MultiOp`, `ZCombine`,
  `Propagate`, `ReplicaApply`, `ExtensionReduced`, `TxnKind` (`Copy`,
  default `Other`), `ResolvedCmd`, `LiveRuntimeConfig`, `ReplicaAck`,
  `XGroupCtx`, `ReplicationSecurity`.

## kevy-resp, kevy-resp-client, kevy-verbs

- kevy-resp: `fuzz::Lcg(pub u64)` → `Lcg::new(seed)` and `state()`;
  `ProtocolError` displays `malformed frame: …` and implements `Error`;
  `ArgvIter<'a, V: ?Sized>`; `Argv` implements `FromIterator<&[u8]>`,
  `Extend<&[u8]>` and `Hash`. `PubsubEvent` lives here now, with
  `into_payload()`. `CmdError`, `ProtocolError`, `NotifyKind`,
  `Strategy`, `FuzzOutcome`, `FuzzResult`, `Summary`, `OpSpec` are
  `#[non_exhaustive]`.
- kevy-resp-client: `parse_url(u)` → `ParsedUrl::parse(u)`;
  `parse_secure_url(u)` → `SecureUrl::parse(u)`; `classify_pubsub(r)` →
  `PubsubEvent::try_from(r)` (re-exported from kevy-resp). `ParsedUrl`
  and `ClientStream` are `#[non_exhaustive]`.
- kevy-verbs: `emit_zrange(res, bool, ..)` → `Scores::{Omitted,
  Included}`; `Claim::new(t, d, bool)` → `aof::Consumer::{Existing,
  Created}`, also in `Effect::RecordRead` and `RecordReads`; `scan_opts`
  returns `ScanOptsError`, `geo::store_search` returns
  `StoreSearchError` (both with `as_wire()`); `reply::store_err_msg(e)` →
  `e.as_wire()`. `Verb` and `ScanOpts` are `#[non_exhaustive]`.

## kevy and kevy-cli

- kevy: `RuntimeState::new(cfg, impl Into<PathBuf>, n) -> Result<_,
  OwnershipError>` (`kevy::OwnershipError`, same text as before);
  `secure::keygen(impl AsRef<Path>)`. `AfterDrain` (now `Clone`, `Copy`,
  `Eq`, `Hash`) and `verb_meta::VerbMeta` are `#[non_exhaustive]`.
- kevy-cli: `doctor::run(c, bool)` / `run_scoped(c, bool, scope)` →
  `OnWarning::{Report, Fail}`; `Scope { .. }` →
  `Scope::default().with_indexes(b).with_views(b)`;
  `run_delete_prefix(c, p, rate, bool)` → `DeleteMode::{Unlink, DryRun}`;
  `run_import(c, src, resume, strict)` → `(c, src,
  ImportStart::{Fresh, Resume}, OnErrorReply::{Count, Abort})`;
  `run_diff` and `run_inspect` take `impl Write` by value;
  `backup::pack` / `unpack` take `impl AsRef<Path>`; `run_backup` /
  `run_restore` → `pack(a, b).map(|_| ())` / `unpack(..)`. `link::Link`
  is sealed. `Source`, `Health`, `Shape` and the report structs are
  `#[non_exhaustive]`. The bare-word tool functions are gone
  ([upgrade guide §12](upgrading-6.4-to-7.0.md#12-kevy-cli-tools-answer-behind---kevy-only)).

## Cluster and election crates

- kevy-elect: `ElectConfig::default().with_hb_interval(..).with_down_after(..).with_election_timeout(..).with_election_backoff(..).with_election_backoff_jitter(..)`;
  `PeerAddr::new(node_id, host, port)`; `SecureLinks::new(local,
  peer_keys)`; `Elector::new(id, peers, addr, role, config, jitter)` →
  `Elector::new(id, peers, addr, role).with_config(config).with_jitter(jitter)`;
  `Transport::spawn*` no longer take `hb_interval` (it is read from the
  elector); `encode(&m)` → `m.encode()`, `decode(buf)` →
  `Message::decode(buf)`. `InboundEvent` is private; `Outbound` and
  `ElectorSnapshot` cannot be built outside the crate; `Message`, `Role`,
  `ElectJitter`, `DecodeError` are `#[non_exhaustive]`; `DecodeError`
  implements `Display` and `Error`.
- kevy-scope: `MigrationError`, `OwnershipError`, `Routing`,
  `MigrationState` are `#[non_exhaustive]`; test a `Routing` with
  `misdirected_target()` / `is_local_writer()`.

## Storage building blocks

- kevy-seg: `ManifestEntry { .. }` →
  `ManifestEntry::new(file, seg_meta).with_meta(meta)`;
  `Manifest::sweep(&self, dir)` → `sweep(&self)`, which sweeps only the
  manifest's own directory (given another directory, 6.4 deleted segments
  that were not its own); `SegBuilder::create`, `Seg::open` and
  `Manifest::open` take `impl AsRef<Path>`. `SegMeta`, `ManifestEntry`,
  `SegError` are `#[non_exhaustive]`.
- kevy-vlog: `VlogRef { .. }` → `VlogRef::new(file_id, offset, len)`;
  `VlogFile::raw_fd()` → `AsRawFd` / `AsFd`; `Vlog::open` takes
  `impl AsRef<Path>`. `VlogRef`, `VlogStats` are `#[non_exhaustive]`.
- kevy-alloc: `SpanMeta`'s fields are methods; `Segment`'s fields →
  `owner()`, `spans()`, `foreign_bytes()`, `foreign_live()`;
  `segment::take_foreign(seg)` → `seg.take_foreign()`; `splice_foreign`
  is an `unsafe` method. `Stats` is `#[non_exhaustive]`.
  `EMPTY_SPAN_HYSTERESIS` is gone: free pages go back to the OS once
  they have gone unused for `PURGE_DELAY` reclaim sweeps, instead of
  every sweep keeping the first four empty spans. `Segment::foreign_bytes()`
  and `foreign_live()` now report the whole owning heap.
- kevy-map: `KevyMap` and `KevySet` implement `IntoIterator` (`IntoIter`,
  `SetIntoIter`) and compare by contents with `PartialEq` / `Eq`.
- kevy-ranktree: `range()` returns `Range`; `FromIterator`, `Extend`, and
  `IntoIterator for &RankTree`.
- kevy-compress: `decode_with(&dict, x)`, `encode_with`,
  `encode_high_with` → `dict.decode(x)`, `dict.encode(x)`,
  `dict.encode_high(x)`; `Corrupt` → `DecodeError` (implements `Error`).

## System and utility crates

- kevy-sys: `Poller::add` / `modify(fd, read, write)` → `(fd, Interest)`
  (`READ`, `WRITE`, `READ | WRITE`, `NONE`); `tcp_listen`,
  `tcp_listen_reuseport`, `unix_listen` → `Socket::tcp_listen`, …;
  `waker()` → `Waker::new()`; `Socket::from_raw_fd` is
  `std::os::fd::FromRawFd` (import the trait); `AsRawFd` and `IntoRawFd`
  are implemented. `Event` is `#[non_exhaustive]`.
- kevy-uring: `FileRead { .. }` → `FileRead::new(fd, offset, len)`
  (`#[non_exhaustive]`); `KernelTimespec` is `Copy`.
- kevy-time: `Civil { y, m, d, h, min, s }` →
  `Civil::from_date(y, m, d)?.with_time(h, min, s)?`; the fields are
  `year()`, `month()`, `day()`, `hour()`, `minute()`, `second()`;
  `civil_from_epoch(s)` → `Civil::from_epoch(s)`; `epoch_from_civil(c)` →
  `c.to_epoch()`; `checked_epoch_from_civil` is gone (the conversion can
  no longer overflow). `Civil` implements `Hash` and `Ord`.
- kevy-noise: `Error` is `#[non_exhaustive]`; `Frames` is `Clone`.
- kevy-crypto: `AuthError` implements `Error`.
- kevy-scalar: `Scalar`, `ScalarError` are `#[non_exhaustive]`.
- kevy-lua: `FlushMode` is `#[non_exhaustive]` and defaults to `Sync`.
- kevy-lua-host: `CurrentTag` is gone; `with_current::<T>` returns `None`
  for the wrong type or for a context already lent out, where 6.4 could
  hand out a second mutable borrow; the `LuaHost<T>` bound moved to the
  impls.
- kevy-bench: `report(label, s)` → `s.report(label)`; `Stats` is
  `#[non_exhaustive]`.
- kevy-tmpdir: `TmpDir::close()` returns `io::Result<()>`.
- The Tauri plugin's `Error` gains `Other(String)`.
