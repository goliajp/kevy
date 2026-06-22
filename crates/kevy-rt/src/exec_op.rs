//! `exec_op` — the cross-shard request-side execution dispatcher. Owned by
//! `Shard` like the rest of `crate::exec`; split into its own file to keep
//! that one under the 500-LOC house rule.

use kevy_resp::Argv;

use crate::Commands;
use crate::message::{DispatchMeta, GatherKind, Gathered, Op, Part, SmallReply};
use crate::shard::Shard;
use kevy_resp::{ArgvView, RespVersion};

impl<C: Commands> Shard<C> {
    /// Execute one resolved single-target command against the local store:
    /// `dispatch_into` the reused `reply_scratch`, copy ≤30 B replies into a
    /// stack-inline [`SmallReply`], then run the meta-driven write
    /// bookkeeping (WATCH bump / AOF / notify / BLOCK wake — see
    /// [`Shard::post_write_housekeeping`]). Borrows `args`, so the local
    /// fallback path dispatches straight off the parse buffer (no owned
    /// `Argv` materialise) and the batched forward path can recycle its
    /// `Argv` after the call.
    pub(crate) fn run_dispatch<A: ArgvView + ?Sized>(
        &mut self,
        args: &A,
        proto: RespVersion,
        meta: DispatchMeta,
    ) -> Part {
        let t0 = self.slowlog_t0();
        self.reply_scratch.clear();
        crate::exec_dispatch::dispatch_proto(
            &self.commands,
            &mut self.store,
            args,
            proto,
            &mut self.reply_scratch,
        );
        let reply = SmallReply::from_slice(&self.reply_scratch);
        self.slowlog_maybe(t0, args);
        if meta.is_write {
            self.post_write_housekeeping(args, meta);
        }
        Part::Reply(reply)
    }

    /// Execute one op against this shard's store, logging mutations to the AOF.
    pub(crate) fn exec_op(&mut self, op: Op) -> Part {
        match op {
            Op::Del(keys) => {
                let n = self.store.del(&keys);
                if n > 0 {
                    for k in &keys {
                        self.store.bump_if_watched(k);
                    }
                    let mut c = Argv::with_capacity(keys.len() + 1, 0);
                    c.push(b"DEL");
                    for k in &keys {
                        c.push(k);
                    }
                    self.log(&c);
                    self.maybe_notify_del(&keys);
                }
                Part::Int(n as i64)
            }
            Op::Exists(keys) => Part::Int(self.store.exists(&keys) as i64),
            Op::Dbsize => Part::Int(self.store.dbsize() as i64),
            Op::Flush => {
                self.store.flushall();
                // Every WATCH against this shard is now invalidated.
                self.store.bump_all_watched();
                let mut c = Argv::with_capacity(1, 8);
                c.push(b"FLUSHALL");
                self.log(&c);
                self.maybe_notify_flush();
                Part::Ok
            }
            Op::MSet(pairs) => {
                for (k, v) in &pairs {
                    self.store.set(k, v.clone(), None, false, false);
                    self.store.bump_if_watched(k);
                }
                if !pairs.is_empty() {
                    let mut c = Argv::with_capacity(pairs.len() * 2 + 1, 0);
                    c.push(b"MSET");
                    for (k, v) in &pairs {
                        c.push(k);
                        c.push(v);
                    }
                    self.log(&c);
                    self.maybe_notify_mset(&pairs);
                }
                Part::Ok
            }
            Op::Gather(kind, keys) => {
                let mut results = Vec::with_capacity(keys.len());
                for k in keys {
                    let g = match kind {
                        GatherKind::Str => {
                            Gathered::Str(self.store.get(&k).ok().flatten().map(|c| c.into_owned()))
                        }
                        GatherKind::Set => match self.store.set_snapshot(&k) {
                            Ok(members) => Gathered::Members(members),
                            Err(_) => Gathered::WrongType,
                        },
                    };
                    results.push((k, g));
                }
                Part::Gathered(results)
            }
            Op::CollectKeys(pat, limit) => {
                Part::Keys(self.store.collect_keys(pat.as_deref(), limit))
            }
            Op::CheckWatch(keys) => {
                // EXEC's pre-execution fan-out: report whether any of
                // `keys` (each carrying the version recorded at WATCH
                // time) is now dirty on this shard. The origin shard
                // ORs the partial results across shards and aborts
                // EXEC if any shard reports `true`.
                let dirty = keys
                    .iter()
                    .any(|(k, v)| self.store.key_version(k) != *v);
                Part::Int(i64::from(dirty))
            }
            Op::Rename { src, dst, nx } => {
                // Same-shard atomic rename. The runtime's start_rename
                // guarantees both keys live on this shard before
                // emitting the Op (cross-shard goes through the v2-3b
                // orchestrator instead — until that lands, it errors
                // out at start_rename).
                use kevy_store::RenameOutcome;
                let outcome = self.store.rename(&src, &dst, nx);
                let renamed = matches!(outcome, RenameOutcome::Renamed);
                let reply = match outcome {
                    RenameOutcome::Renamed if nx => b":1\r\n".to_vec(),
                    RenameOutcome::Renamed => b"+OK\r\n".to_vec(),
                    RenameOutcome::DstExists => b":0\r\n".to_vec(),
                    RenameOutcome::NoSuchSrc => b"-ERR no such key\r\n".to_vec(),
                };
                if renamed {
                    // AOF + WATCH bump for both src (deleted) and dst (created).
                    self.store.bump_if_watched(&src);
                    self.store.bump_if_watched(&dst);
                    if self.aof.is_some() {
                        let mut c = Argv::with_capacity(3, 0);
                        c.push(if nx { b"RENAMENX" } else { b"RENAME" });
                        c.push(&src);
                        c.push(&dst);
                        self.log(&c);
                    }
                    // Keyspace notifications: generic class, two events
                    // (`rename_from` on src, `rename_to` on dst) per
                    // Redis events.c convention.
                    if !self.notify_flags.is_empty() && self.notify_flags.generic {
                        self.notify_keyspace_event(b"rename_from", &src);
                        self.notify_keyspace_event(b"rename_to", &dst);
                    }
                }
                Part::Reply(SmallReply::from_vec(reply))
            }
            Op::RenameTake(src) => {
                // Step 1 of cross-shard RENAME: atomically take the
                // entry out of this shard. The orchestrator on the
                // origin shard chains the value into a follow-up
                // `Op::RenamePut` on the destination shard.
                match self.store.take_with_ttl(&src) {
                    Some((value, ttl_ms)) => {
                        self.store.bump_if_watched(&src);
                        Part::RenameTaken { value, ttl_ms }
                    }
                    None => Part::RenameNoSuchSrc,
                }
            }
            Op::RenamePut {
                dst,
                value,
                ttl_ms,
                nx,
            } => {
                // Step 2 of cross-shard RENAME. If NX is set and dst
                // already exists on this shard, refuse the put. The
                // orchestrator decides whether to surface `:0` (RENAMENX
                // blocked) — RENAME (non-NX) always succeeds here.
                if nx && self.store.key_exists(&dst) {
                    // NX-refused: hand the source value back so the
                    // orchestrator can restore it on src's shard.
                    return Part::RenamePutDone {
                        refused: Some((value, ttl_ms)),
                    };
                }
                self.store.put_with_ttl(dst.clone(), value, ttl_ms);
                self.store.bump_if_watched(&dst);
                // AOF / cross-shard RENAME durability is deferred —
                // a faithful AOF replay would need to serialise the
                // value through MIGRATE/RESTORE-style binary frames.
                // For v2-3b, document the gap: cross-shard RENAME
                // works in-memory but is not replayed through AOF.
                Part::RenamePutDone { refused: None }
            }
            Op::CollectWatchVersions(keys) => {
                // WATCH's fan-out: register each key in this shard's
                // version tracker and report its current version. The
                // origin shard stashes (key, version) pairs into the
                // conn's watched set; EXEC checks against these via
                // [`Op::CheckWatch`].
                let mut out = Vec::with_capacity(keys.len());
                for k in &keys {
                    out.push((k.clone(), self.store.record_watch(k)));
                }
                Part::WatchVersions(out)
            }
            Op::Save => {
                // v1.25.x A.3 follow-up: `SAVE` was previously a synchronous
                // `save_snapshot(&self.store, &path)` on the shard thread,
                // holding the reactor for the entire RDB serialize + disk
                // write — the last shard-blocker on the persistence path
                // (BGSAVE/BGREWRITEAOF/auto-rewrite migrated to the per-shard
                // `PersistWorker` in `8cc2bcf` 2026-06-11). It now delegates
                // to [`Self::start_bg_save`]: freeze a COW [`SnapshotView`]
                // on this thread (O(n) shallow — 8 ns/entry, see
                // `kevy_store::Store::collect_snapshot`), hand off the
                // serialize + fsync + rename to the per-shard persist
                // worker, and reply `+OK` immediately. The AOF reset that
                // used to be `aof.truncate()` after a successful save is
                // now the `aof_reset` path inside `start_bg_save` →
                // `poll_persist_done` (COW tee + `finish_concurrent_rewrite`
                // → atomic swap to the post-collect log).
                //
                // **Semantic change**: SAVE no longer blocks the *client*
                // until the snapshot is durable on disk. The reply is `+OK`
                // as soon as the COW view is frozen; durability lands
                // microseconds-to-seconds later via the persist worker's
                // completion, committed in the next tick's
                // `poll_persist_done` rename. Workflows that depend on
                // "SAVE returned → safe to rsync the .rdb" must now wait
                // for the next `LASTSAVE` increment / read the file
                // exists at `dump-{i}.rdb` (the worker's rename is atomic).
                // The previous behaviour also already pre-committed this
                // direction for the multi-shard case: a multi-shard SAVE
                // already aggregated `+OK` after each shard's local
                // save, with no cross-shard durability barrier — making
                // the conn block per-shard for completion via a deferred-
                // reply Inbound channel is a larger refactor (Part::Defer
                // through fold + cross-shard Response holdback) and was
                // explicitly out-of-scope for the unblock-the-reactor goal.
                //
                // The in-flight-already case still short-circuits: the
                // existing log + `Part::Ok` matches the previous
                // `SAVE-during-BGSAVE` behaviour, and `start_bg_save`'s
                // own busy check is the second line of defence.
                self.start_bg_save();
                Part::Ok
            }
            Op::SlowlogGet => Part::SlowlogEntries(self.slowlog.buf.iter().cloned().collect()),
            Op::SlowlogLen => Part::Int(self.slowlog.buf.len() as i64),
            Op::SlowlogReset => {
                self.slowlog.buf.clear();
                Part::Ok
            }
            Op::XReadOne { index, argv, write } => {
                // Single-stream non-blocking XREAD/XREADGROUP on the
                // stream's owning shard (`$` resolves to this shard's
                // last_id). Neither has a RESP3 override (always a RESP2
                // array), so the reply is one of: `*1\r\n<element>` (data) /
                // `*-1\r\n` (empty) / `-ERR…`.
                let reply = self.commands.dispatch(&mut self.store, &argv);
                // The XREADGROUP form mutates group state (PEL /
                // last-delivered) — run the same post-write housekeeping
                // (AOF, WATCH bump, notify) the Route::Single path gets,
                // against the rewritten single-stream argv. `build_xread_
                // targets` emits a fixed `… STREAMS <key> <cursor>` tail, so
                // the key is always the second-to-last arg — derive it
                // directly. A token search for "STREAMS" would mis-fire on a
                // group/consumer literally named "streams" (a legal Redis
                // name) and point the WATCH bump / notify at the wrong key.
                if write {
                    let key_idx = (argv.len() >= 2).then(|| (argv.len() - 2) as u8);
                    let meta = DispatchMeta { is_write: true, wake_idx: None, key_idx };
                    self.post_write_housekeeping(&argv, meta);
                }
                let element = if reply.starts_with(b"*1\r\n") {
                    Some(reply[4..].to_vec()) // strip the array wrapper
                } else if reply.first() == Some(&b'-') {
                    Some(reply) // error: carried verbatim, origin surfaces it
                } else {
                    None // `*-1` — this stream had nothing
                };
                Part::XReadElement { index, element }
            }
            // COW background save: freeze the view here (short pause),
            // serialize + spill on the persist worker; the tick commits.
            Op::BgSave => {
                self.start_bg_save();
                Part::Ok
            }
            Op::RewriteAof => {
                // Each shard rewrites its own AOF via a COW view dumped on
                // the persist worker (the tick swaps it in). No-op if AOF
                // is disabled (Redis returns "ERR" in that case; kevy
                // returns +OK to keep the multi-shard reply aggregation
                // simple — documented in BGREWRITEAOF's reply).
                self.start_bg_rewrite();
                Part::Ok
            }
        }
    }

    // The WATCH version bump reads `DispatchMeta::key_idx` directly in
    // `Shard::run_dispatch` above — the old `bump_watch_for_dispatch`
    // re-ran the full `Commands::route` verb walk per write and is gone.
}
