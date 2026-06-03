//! `exec_op` — the cross-shard request-side execution dispatcher. Owned by
//! `Shard` like the rest of `crate::exec`; split into its own file to keep
//! that one under the 500-LOC house rule.

use kevy_persist::save_snapshot;
use kevy_resp::Argv;

use crate::Commands;
use crate::message::{GatherKind, Gathered, Op, Part};
use crate::shard::Shard;

impl<C: Commands> Shard<C> {
    /// Execute one op against this shard's store, logging mutations to the AOF.
    pub(crate) fn exec_op(&mut self, op: Op) -> Part {
        match op {
            Op::Dispatch(args) => {
                let reply = self.commands.dispatch(&mut self.store, &args);
                // Only classify writes when there's an AOF to log them to —
                // otherwise `is_write` (+ its verb fold) is pure waste, and the
                // cache-only / `--no-aof` path is hot.
                if self.aof.is_some() && self.commands.is_write(&args) {
                    self.log(&args);
                }
                Part::Reply(reply)
            }
            Op::Del(keys) => {
                let n = self.store.del(&keys);
                if n > 0 {
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
                let mut c = Argv::with_capacity(1, 8);
                c.push(b"FLUSHALL");
                self.log(&c);
                Part::Ok
            }
            Op::MSet(pairs) => {
                for (k, v) in &pairs {
                    self.store.set(k, v.clone(), None, false, false);
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
}
