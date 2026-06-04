//! `exec_op` — the cross-shard request-side execution dispatcher. Owned by
//! `Shard` like the rest of `crate::exec`; split into its own file to keep
//! that one under the 500-LOC house rule.

use kevy_persist::save_snapshot;
use kevy_resp::{Argv, ArgvView};

use crate::Commands;
use crate::Route;
use crate::message::{GatherKind, Gathered, Op, Part};
use crate::shard::Shard;

impl<C: Commands> Shard<C> {
    /// Execute one op against this shard's store, logging mutations to the AOF.
    pub(crate) fn exec_op(&mut self, op: Op) -> Part {
        match op {
            Op::Dispatch(args) => {
                let reply = self.commands.dispatch(&mut self.store, &args);
                // Write-side bookkeeping: AOF logging + WATCH version
                // bump. Both gated on `is_write` so the cache-only path
                // (no AOF + no WATCH-ed keys) pays nothing beyond one
                // verb-table lookup. The WATCH bump is also gated inside
                // `bump_if_watched` — it's an empty-map lookup when no
                // key on this shard has ever been WATCH-ed.
                if self.commands.is_write(&args) {
                    self.bump_watch_for_dispatch(&args);
                    if self.aof.is_some() {
                        self.log(&args);
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
