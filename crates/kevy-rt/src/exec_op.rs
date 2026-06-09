//! `exec_op` — the cross-shard request-side execution dispatcher. Owned by
//! `Shard` like the rest of `crate::exec`; split into its own file to keep
//! that one under the 500-LOC house rule.

use kevy_persist::save_snapshot;
use kevy_resp::{Argv, ArgvView};
use std::time::Instant;

use crate::Commands;
use crate::Route;
use crate::message::{GatherKind, Gathered, Op, Part};
use crate::shard::Shard;
use kevy_resp::RespVersion;

impl<C: Commands> Shard<C> {
    /// Execute one op against this shard's store, logging mutations to the AOF.
    pub(crate) fn exec_op(&mut self, op: Op) -> Part {
        match op {
            Op::Dispatch(args, proto) => {
                // Per-cmd proto picks the reply encoder. V2 hot path
                // resolves to a single `dispatch` call (the existing
                // bench-measured path); the V3 arm only fires after a
                // HELLO 3 negotiation upstream.
                // SLOWLOG OFF (`slower_than_micros < 0`) skips the
                // clock pair entirely.
                let t0 = if self.slowlog.slower_than_micros >= 0 {
                    Some(Instant::now())
                } else {
                    None
                };
                let reply = match proto {
                    RespVersion::V2 => self.commands.dispatch(&mut self.store, &args),
                    RespVersion::V3 => self.commands.dispatch_resp3(&mut self.store, &args),
                };
                if let Some(t0) = t0 {
                    let elapsed = t0.elapsed().as_micros().min(u64::MAX as u128) as u64;
                    self.slowlog_record(&args, elapsed);
                }
                // Write-side bookkeeping: AOF logging + WATCH version
                // bump. Both gated on `is_write` so the cache-only path
                // (no AOF + no WATCH-ed keys) pays nothing beyond one
                // verb-table lookup. The WATCH bump is also gated inside
                // `bump_if_watched` — it's an empty-map lookup when no
                // key on this shard has ever been WATCH-ed.
                if self.commands.is_write(&args) {
                    self.bump_watch_for_dispatch(&args);
                    if self.aof.is_some() {
                        self.log_write(&args);
                    }
                    // Keyspace notification fan-out. Empty-flags
                    // short-circuit inside maybe_notify_dispatch keeps
                    // the OFF hot path at one bool-OR per write.
                    self.maybe_notify_dispatch(&args);
                    // BLOCK wake: a forwarded LPUSH/RPUSH/XADD lands here on
                    // the key's owning shard, where any waiter (in-shard or
                    // cross-shard) is registered. The local fast-path write
                    // wakes via `post_write_housekeeping` instead.
                    if let Some(idx) = self.commands.wake_idx(&args)
                        && let Some(key) = args.get(idx as usize).map(<[u8]>::to_vec)
                    {
                        self.wake_key(&key);
                    }
                }
                Part::Reply(reply)
            }
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
                self.store.flush();
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
                            Gathered::Str(self.store.get(&k).ok().flatten().map(|v| v.to_vec()))
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
                Part::Int(dirty as i64)
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
                Part::Reply(reply)
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
                let path = self.snapshot_path();
                match save_snapshot(&self.store, &path) {
                    // Snapshot now captures full state → reset the AOF.
                    Ok(()) => {
                        if let Some(aof) = &mut self.aof
                            && let Err(e) = aof.truncate()
                        {
                            eprintln!("kevy: shard {} aof truncate failed: {e}", self.id);
                        }
                    }
                    Err(e) => {
                        eprintln!(
                            "kevy: shard {} failed to save {}: {e}",
                            self.id,
                            path.display()
                        )
                    }
                }
                Part::Ok
            }
            Op::SlowlogGet => Part::SlowlogEntries(self.slowlog.buf.iter().cloned().collect()),
            Op::SlowlogLen => Part::Int(self.slowlog.buf.len() as i64),
            Op::SlowlogReset => {
                self.slowlog.buf.clear();
                Part::Ok
            }
            Op::XReadOne { index, argv } => {
                // Single-stream non-blocking XREAD on the stream's owning
                // shard (`$` resolves to this shard's last_id). XREAD has no
                // RESP3 override (always a RESP2 array), so the reply is one
                // of: `*1\r\n<element>` (data) / `*-1\r\n` (empty) / `-ERR…`.
                let reply = self.commands.dispatch(&mut self.store, &argv);
                let element = if reply.starts_with(b"*1\r\n") {
                    Some(reply[4..].to_vec()) // strip the array wrapper
                } else if reply.first() == Some(&b'-') {
                    Some(reply) // error: carried verbatim, origin surfaces it
                } else {
                    None // `*-1` — this stream had nothing
                };
                Part::XReadElement { index, element }
            }
            Op::RewriteAof => {
                // Each shard rewrites its own AOF in place. No-op if AOF is
                // disabled (Redis returns "ERR" in that case; v1.0 returns
                // +OK to keep the multi-shard reply aggregation simple — the
                // disabled-AOF case is documented in BGREWRITEAOF's reply).
                if let Some(aof) = &mut self.aof
                    && let Err(e) = aof.rewrite_from(&self.store)
                {
                    eprintln!("kevy: shard {} aof rewrite failed: {e}", self.id,);
                }
                Part::Ok
            }
        }
    }

    /// Resolve which arg index carries the key for a write `Op::Dispatch`,
    /// then bump that key's WATCH version. Read-only commands and keyless
    /// admin verbs (already filtered by `is_write`) never reach here. The
    /// route lookup is one verb-table dispatch (~5 ns); inside-store the
    /// bump is one HashMap::get_mut (no insert) — empty when no key on
    /// this shard has ever been WATCH-ed.
    pub(crate) fn bump_watch_for_dispatch<A: ArgvView + ?Sized>(&mut self, args: &A) {
        if let Route::Single(idx) = self.commands.route(args)
            && idx < args.len()
        {
            self.store.bump_if_watched(&args[idx]);
        }
    }
}
