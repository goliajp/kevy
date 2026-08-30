//! Non-atomic batched-fsync write queue: `Store::pipeline`.
//!
//! Builder-style queue: enqueue any number of writes via fluent
//! methods, then `commit()` applies them in queue order. Per-shard
//! AOF appends are batched into one fsync per shard at commit
//! time, cutting fsync cost from `N` to `min(N, shard_count)`.
//!
//! `Pipeline` is not atomic — each op acquires its own per-shard
//! write lock as it is applied, so other writers see intermediate
//! states. For transactional semantics use
//! [`Store::atomic`](crate::Store::atomic).

use crate::KevyResult;

use crate::store::Store;

/// Builder-style write queue. Returned by [`Store::pipeline`]; call
/// fluent methods to enqueue + `commit()` to apply with batched
/// AOF fsync.
pub struct Pipeline<'a> {
    store: &'a Store,
    ops: Vec<PendingOp>,
}

enum PendingOp {
    Set { key: Vec<u8>, value: Vec<u8> },
    Del { keys: Vec<Vec<u8>> },
    Incr { key: Vec<u8> },
    IncrBy { key: Vec<u8>, delta: i64 },
    HSet { key: Vec<u8>, pairs: Vec<(Vec<u8>, Vec<u8>)> },
    HDel { key: Vec<u8>, fields: Vec<Vec<u8>> },
    HIncrBy { key: Vec<u8>, field: Vec<u8>, delta: i64 },
    ZAdd { key: Vec<u8>, pairs: Vec<(f64, Vec<u8>)> },
    ZAddFlags { key: Vec<u8>, pairs: Vec<(f64, Vec<u8>)>, flags: kevy_store::ZaddFlags },
    ZRem { key: Vec<u8>, members: Vec<Vec<u8>> },
    ZIncrBy { key: Vec<u8>, delta: f64, member: Vec<u8> },
    SAdd { key: Vec<u8>, members: Vec<Vec<u8>> },
    SRem { key: Vec<u8>, members: Vec<Vec<u8>> },
    LPush { key: Vec<u8>, values: Vec<Vec<u8>> },
    RPush { key: Vec<u8>, values: Vec<Vec<u8>> },
}

impl<'a> Pipeline<'a> {
    pub(crate) fn new(store: &'a Store) -> Self {
        Self { store, ops: Vec::new() }
    }

    /// Number of ops queued so far.
    pub fn len(&self) -> usize {
        self.ops.len()
    }

    /// `true` when no ops are queued.
    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }

    // ---- fluent enqueue --------------------------------------------

    /// Queue `SET key value`. Nothing is applied (or AOF-logged)
    /// until [`commit`](Self::commit) — true of every enqueue method.
    pub fn set(mut self, key: &[u8], value: &[u8]) -> Self {
        self.ops.push(PendingOp::Set { key: key.to_vec(), value: value.to_vec() });
        self
    }

    /// Queue `DEL key [key ...]`.
    pub fn del(mut self, keys: &[&[u8]]) -> Self {
        self.ops.push(PendingOp::Del { keys: keys.iter().map(|k| k.to_vec()).collect() });
        self
    }

    /// Queue `INCR key` (commit errors if the value is not an integer).
    pub fn incr(mut self, key: &[u8]) -> Self {
        self.ops.push(PendingOp::Incr { key: key.to_vec() });
        self
    }

    /// Queue `INCRBY key delta`; negative `delta` does DECR-style work.
    pub fn incr_by(mut self, key: &[u8], delta: i64) -> Self {
        self.ops.push(PendingOp::IncrBy { key: key.to_vec(), delta });
        self
    }

    /// Queue `HSET key field value [field value ...]`.
    pub fn hset(mut self, key: &[u8], pairs: &[(&[u8], &[u8])]) -> Self {
        self.ops.push(PendingOp::HSet {
            key: key.to_vec(),
            pairs: pairs.iter().map(|(f, v)| (f.to_vec(), v.to_vec())).collect(),
        });
        self
    }

    /// Queue `HDEL key field [field ...]`.
    pub fn hdel(mut self, key: &[u8], fields: &[&[u8]]) -> Self {
        self.ops.push(PendingOp::HDel {
            key: key.to_vec(),
            fields: fields.iter().map(|f| f.to_vec()).collect(),
        });
        self
    }

    /// Queue `HINCRBY key field delta`.
    pub fn hincrby(mut self, key: &[u8], field: &[u8], delta: i64) -> Self {
        self.ops.push(PendingOp::HIncrBy { key: key.to_vec(), field: field.to_vec(), delta });
        self
    }

    /// Queue `ZADD key score member [score member ...]`.
    pub fn zadd(mut self, key: &[u8], pairs: &[(f64, &[u8])]) -> Self {
        self.ops.push(PendingOp::ZAdd {
            key: key.to_vec(),
            pairs: pairs.iter().map(|(s, m)| (*s, m.to_vec())).collect(),
        });
        self
    }

    /// Flags-aware `ZADD` — e.g. the `GT` monotonic-heal form.
    pub fn zadd_flags(
        mut self,
        key: &[u8],
        pairs: &[(f64, &[u8])],
        flags: kevy_store::ZaddFlags,
    ) -> Self {
        self.ops.push(PendingOp::ZAddFlags {
            key: key.to_vec(),
            pairs: pairs.iter().map(|(s, m)| (*s, m.to_vec())).collect(),
            flags,
        });
        self
    }

    /// Queue `ZREM key member [member ...]`.
    pub fn zrem(mut self, key: &[u8], members: &[&[u8]]) -> Self {
        self.ops.push(PendingOp::ZRem {
            key: key.to_vec(),
            members: members.iter().map(|m| m.to_vec()).collect(),
        });
        self
    }

    /// Queue `ZINCRBY key delta member`.
    pub fn zincrby(mut self, key: &[u8], delta: f64, member: &[u8]) -> Self {
        self.ops.push(PendingOp::ZIncrBy { key: key.to_vec(), delta, member: member.to_vec() });
        self
    }

    /// Queue `SADD key member [member ...]`.
    pub fn sadd(mut self, key: &[u8], members: &[&[u8]]) -> Self {
        self.ops.push(PendingOp::SAdd {
            key: key.to_vec(),
            members: members.iter().map(|m| m.to_vec()).collect(),
        });
        self
    }

    /// Queue `SREM key member [member ...]`.
    pub fn srem(mut self, key: &[u8], members: &[&[u8]]) -> Self {
        self.ops.push(PendingOp::SRem {
            key: key.to_vec(),
            members: members.iter().map(|m| m.to_vec()).collect(),
        });
        self
    }

    /// Queue `LPUSH key value [value ...]`.
    pub fn lpush(mut self, key: &[u8], values: &[&[u8]]) -> Self {
        self.ops.push(PendingOp::LPush {
            key: key.to_vec(),
            values: values.iter().map(|v| v.to_vec()).collect(),
        });
        self
    }

    /// Queue `RPUSH key value [value ...]`.
    pub fn rpush(mut self, key: &[u8], values: &[&[u8]]) -> Self {
        self.ops.push(PendingOp::RPush {
            key: key.to_vec(),
            values: values.iter().map(|v| v.to_vec()).collect(),
        });
        self
    }

    /// Apply every queued op in order. Each op acquires its own
    /// per-shard write lock — other writers see intermediate states
    /// between ops; for transactional semantics use [`Store::atomic`]
    /// instead. AOF appends batch into one fsync per shard.
    pub fn commit(mut self) -> KevyResult<()> {
        let ops = std::mem::take(&mut self.ops);
        for op in ops {
            self.apply_one(op)?;
        }
        Ok(())
    }

    // fn-length exemption: pure data-driven op match table — one flat
    // arm per PendingOp variant, only arg plumbing + one store call.
    // LOC-WAIVER: data-driven op dispatch table — one store-call arm per PendingOp variant.
    fn apply_one(&self, op: PendingOp) -> KevyResult<()> {
        match op {
            PendingOp::Set { key, value } => {
                self.store.set(&key, &value)?;
            }
            PendingOp::Del { keys } => {
                let refs: Vec<&[u8]> = keys.iter().map(|k| k.as_slice()).collect();
                self.store.del(&refs)?;
            }
            PendingOp::Incr { key } => {
                self.store.incr(&key)?;
            }
            PendingOp::IncrBy { key, delta } => {
                self.store.incr_by(&key, delta)?;
            }
            PendingOp::HSet { key, pairs } => {
                let refs: Vec<(&[u8], &[u8])> =
                    pairs.iter().map(|(f, v)| (f.as_slice(), v.as_slice())).collect();
                self.store.hset(&key, &refs)?;
            }
            PendingOp::HDel { key, fields } => {
                let refs: Vec<&[u8]> = fields.iter().map(|f| f.as_slice()).collect();
                self.store.hdel(&key, &refs)?;
            }
            PendingOp::HIncrBy { key, field, delta } => {
                self.store.hincrby(&key, &field, delta)?;
            }
            PendingOp::ZAdd { key, pairs } => {
                let refs: Vec<(f64, &[u8])> =
                    pairs.iter().map(|(s, m)| (*s, m.as_slice())).collect();
                self.store.zadd(&key, &refs)?;
            }
            PendingOp::ZAddFlags { key, pairs, flags } => {
                let refs: Vec<(f64, &[u8])> =
                    pairs.iter().map(|(s, m)| (*s, m.as_slice())).collect();
                self.store.zadd_flags(&key, &refs, flags)?;
            }
            PendingOp::ZRem { key, members } => {
                let refs: Vec<&[u8]> = members.iter().map(|m| m.as_slice()).collect();
                self.store.zrem(&key, &refs)?;
            }
            PendingOp::ZIncrBy { key, delta, member } => {
                self.store.zincrby(&key, delta, &member)?;
            }
            PendingOp::SAdd { key, members } => {
                let refs: Vec<&[u8]> = members.iter().map(|m| m.as_slice()).collect();
                self.store.sadd(&key, &refs)?;
            }
            PendingOp::SRem { key, members } => {
                let refs: Vec<&[u8]> = members.iter().map(|m| m.as_slice()).collect();
                self.store.srem(&key, &refs)?;
            }
            PendingOp::LPush { key, values } => {
                let refs: Vec<&[u8]> = values.iter().map(|v| v.as_slice()).collect();
                self.store.lpush(&key, &refs)?;
            }
            PendingOp::RPush { key, values } => {
                let refs: Vec<&[u8]> = values.iter().map(|v| v.as_slice()).collect();
                self.store.rpush(&key, &refs)?;
            }
        }
        Ok(())
    }
}

impl Store {
    /// Begin a [`Pipeline`] — fluent write queue. Add ops via
    /// `.set(...).hset(...).zadd(...)` then call `.commit()`.
    pub fn pipeline(&self) -> Pipeline<'_> {
        Pipeline::new(self)
    }
}

/// Parity manifest: command names `Pipeline` implements.
/// Cross-checked against `kevy_resp::ops_table` in
/// `store_tests_op_table.rs` — update BOTH when adding an op.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const PIPELINE_OPS: &[&str] = &[
    "SET", "DEL", "INCR", "INCRBY", "HSET", "HDEL", "HINCRBY", "ZADD", "ZREM", "ZINCRBY", "SADD",
    "SREM", "LPUSH", "RPUSH",
];
