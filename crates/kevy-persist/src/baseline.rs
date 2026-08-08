//! The rewrite-baseline estimator: what the live keyspace WOULD weigh as
//! a fresh rewrite image, without writing one.
//!
//! Why it exists: `Aof::open` can only see the file, so it initialises
//! `size_at_last_rewrite` to the file's size. For a long-lived server that
//! is Redis's own semantic (the growth rule fires once the log doubles
//! past what boot loaded). For a short-lived process reusing the same
//! directory — a CLI, a git hook, a cron job — it is a trap: every run
//! resets the baseline to the ever-larger file, appends a few KB, and
//! exits before the +pct% rule can ever fire, so the log grows without
//! bound while the live keyspace stays tiny. Anchoring the baseline to
//! the live image's estimated size after replay restores the growth
//! rule's real meaning ("the log is pct% history") across processes.

use crate::SnapshotSource;
use std::io::{self, Write};

/// A `Write` that counts and discards. The estimator serialises the
/// keyspace through the same emitters a real rewrite uses, so the count
/// is the rewrite image's size — O(keys) time, O(1) memory.
struct CountWriter(u64);

impl Write for CountWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0 += buf.len() as u64;
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Estimated size in bytes of a fresh rewrite image of `src` (magic +
/// command stream + hash-TTL frames). Cold (tiered) stubs and their
/// trailing segment frames are not counted — the estimate errs low,
/// which for the baseline's purpose is the safe direction (at worst one
/// rewrite fires earlier than strictly needed).
pub fn estimate_rewrite_size<S: SnapshotSource>(src: &S) -> u64 {
    let mut w = CountWriter(crate::record::AOF2_MAGIC.len() as u64);
    let mut scratch = Vec::new();
    src.for_each_entry(|key, value, ttl_ms| {
        if matches!(value, kevy_store::Value::Cold(_)) {
            return;
        }
        let _ = crate::rewrite_fmt::write_value_as_commands(
            &mut w,
            key,
            value,
            ttl_ms,
            crate::AofFormat::V2,
            &mut scratch,
        );
    });
    src.for_each_hash_ttl(|key, field, deadline_ms| {
        let ms = deadline_ms.to_string();
        let mut argv = kevy_resp::Argv::with_capacity(6, 0);
        argv.push(b"HPEXPIREAT");
        argv.push(key);
        argv.push(ms.as_bytes());
        argv.push(b"FIELDS");
        argv.push(b"1");
        argv.push(field);
        let _ = crate::rewrite_fmt::emit(&mut w, &argv, crate::AofFormat::V2, &mut scratch);
    });
    w.0
}
