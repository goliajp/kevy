//! `MEMORY` subcommands: `USAGE <key>` and `STATS`.
//!
//! Both surface the same accounting `kevy-store` maintains for `maxmemory`
//! enforcement, so a client can read the exact numbers eviction is gating
//! against. `STATS` is the trimmed Redis shape — only the fields valkey CLI
//! clients actually parse, since we don't carry RDB/COW/replication state.

use kevy_config::Config;
use kevy_resp::{ArgvView, encode_array_len, encode_bulk, encode_error, encode_integer, encode_null_bulk};
use kevy_store::{ENTRY_OVERHEAD, Store};

use super::{eviction_str, wrong_args};

pub(crate) fn cmd_memory<A: ArgvView + ?Sized>(
    cfg: &Config,
    store: &Store,
    args: &A,
    out: &mut Vec<u8>,
) {
    let Some(sub) = args.get(1) else {
        return wrong_args(out, "memory");
    };
    let sub_upper = sub.to_ascii_uppercase();
    match sub_upper.as_slice() {
        b"USAGE" => cmd_memory_usage(store, args, out),
        b"STATS" => cmd_memory_stats(cfg, store, out),
        b"DOCTOR" => {
            // Redis returns a free-form diagnostic string. v1.0 ships a
            // canonical "no issues" body so clients that auto-call DOCTOR on
            // INFO don't think the server is buggy. Wave 2/3 may surface real
            // findings (fragmentation, high evict rate, etc.).
            encode_bulk(out, b"Sam, I detected a few issues in this Kevy instance memory implants:\r\n\r\n * No issues detected. Memory looks fine.\r\n");
        }
        b"PURGE" => {
            // No-op — kevy doesn't use jemalloc, so there's no arena to
            // purge. Reply +OK so client code that calls PURGE after large
            // deletes (a common Redis pattern) keeps working.
            kevy_resp::encode_simple_string(out, "OK");
        }
        b"MALLOC-STATS" => {
            encode_bulk(out, b"kevy uses the system allocator; no per-arena stats.\r\n");
        }
        _ => {
            let shown = String::from_utf8_lossy(sub);
            encode_error(out, &format!(
                "ERR Unknown MEMORY subcommand or wrong number of arguments for '{}'",
                shown.to_lowercase()
            ));
        }
    }
}

/// `MEMORY USAGE <key> [SAMPLES count]` — the `SAMPLES` arg is accepted for
/// parity but ignored; our accounting is already exact-per-entry.
fn cmd_memory_usage<A: ArgvView + ?Sized>(store: &Store, args: &A, out: &mut Vec<u8>) {
    if args.len() < 3 {
        return wrong_args(out, "memory|usage");
    }
    let key = &args[2];
    match store.estimate_key_bytes(key) {
        Some(b) => encode_integer(out, b as i64),
        None => encode_null_bulk(out),
    }
}

/// `MEMORY STATS` — flat `[k1, v1, k2, v2, ...]` array of the fields valkey
/// clients actually consult. Strings as bulk, numbers as integers.
fn cmd_memory_stats(cfg: &Config, store: &Store, out: &mut Vec<u8>) {
    let pairs: [(&[u8], StatValue<'_>); 8] = [
        (b"peak.allocated", StatValue::Int(store.used_memory_peak() as i64)),
        (b"total.allocated", StatValue::Int(store.used_memory() as i64)),
        (
            b"keys.count",
            StatValue::Int(store.dbsize() as i64),
        ),
        (
            b"keys.bytes-per-key",
            StatValue::Int(avg_bytes_per_key(store)),
        ),
        (b"maxmemory", StatValue::Int(cfg.memory.maxmemory as i64)),
        (
            b"maxmemory.policy",
            StatValue::Bulk(eviction_str(cfg.memory.maxmemory_policy).as_bytes()),
        ),
        (
            b"evicted.keys",
            StatValue::Int(store.evictions_total() as i64),
        ),
        (
            b"entry.overhead",
            StatValue::Int(ENTRY_OVERHEAD as i64),
        ),
    ];
    encode_array_len(out, (pairs.len() * 2) as i64);
    for (k, v) in &pairs {
        encode_bulk(out, k);
        match v {
            StatValue::Int(n) => encode_integer(out, *n),
            StatValue::Bulk(b) => encode_bulk(out, b),
        }
    }
}

enum StatValue<'a> {
    Int(i64),
    Bulk(&'a [u8]),
}

fn avg_bytes_per_key(store: &Store) -> i64 {
    let n = store.dbsize();
    if n == 0 {
        return 0;
    }
    (store.used_memory() as i64) / (n as i64)
}

/// Pretty-print a byte count using IEC suffixes (matches Redis output, e.g.
/// `used_memory_human:1.50M`). Single decimal place; rounds half-to-even.
pub(crate) fn format_bytes_human(bytes: u64) -> String {
    const UNITS: [(&str, u64); 4] = [
        ("G", 1024 * 1024 * 1024),
        ("M", 1024 * 1024),
        ("K", 1024),
        ("B", 1),
    ];
    for (suffix, scale) in UNITS {
        if bytes >= scale {
            if suffix == "B" {
                return format!("{bytes}B");
            }
            let scaled = bytes as f64 / scale as f64;
            return format!("{scaled:.2}{suffix}");
        }
    }
    format!("{bytes}B")
}

#[cfg(test)]
mod tests {
    use super::*;
    use kevy_resp::Argv;

    #[test]
    fn human_format_matches_redis_style() {
        assert_eq!(format_bytes_human(0), "0B");
        assert_eq!(format_bytes_human(512), "512B");
        assert_eq!(format_bytes_human(1024), "1.00K");
        assert_eq!(format_bytes_human(1536), "1.50K");
        assert_eq!(format_bytes_human(1024 * 1024), "1.00M");
        assert_eq!(format_bytes_human(2 * 1024 * 1024 * 1024), "2.00G");
    }

    #[test]
    fn memory_usage_returns_nil_for_absent_key() {
        let store = Store::new();
        let mut a = Argv::default();
        a.push(b"MEMORY");
        a.push(b"USAGE");
        a.push(b"missing");
        let mut out = Vec::new();
        cmd_memory_usage(&store, &a, &mut out);
        assert_eq!(out, b"$-1\r\n");
    }

    fn argv(parts: &[&[u8]]) -> Argv {
        let mut a = Argv::default();
        for p in parts {
            a.push(p);
        }
        a
    }

    #[test]
    fn memory_usage_returns_integer_for_present_key() {
        let mut store = Store::new();
        store.set(b"k", b"v".to_vec(), None, false, false);
        let a = argv(&[b"MEMORY", b"USAGE", b"k"]);
        let mut out = Vec::new();
        cmd_memory_usage(&store, &a, &mut out);
        assert!(out.starts_with(b":"), "expected integer reply, got {out:?}");
        assert!(out.ends_with(b"\r\n"));
    }

    #[test]
    fn memory_usage_wrong_arity() {
        let store = Store::new();
        let a = argv(&[b"MEMORY", b"USAGE"]);
        let mut out = Vec::new();
        cmd_memory_usage(&store, &a, &mut out);
        assert!(out.starts_with(b"-ERR wrong number of arguments"));
    }

    #[test]
    fn memory_top_level_dispatches_each_subcommand() {
        let cfg = Config::default();
        let store = Store::new();
        // DOCTOR — canonical no-issues body
        let mut out = Vec::new();
        cmd_memory(&cfg, &store, &argv(&[b"MEMORY", b"DOCTOR"]), &mut out);
        assert!(
            out.starts_with(b"$") && out.windows(5).any(|w| w == b"issue"),
            "DOCTOR should return a bulk diagnostic; got {out:?}"
        );
        // PURGE — +OK
        out.clear();
        cmd_memory(&cfg, &store, &argv(&[b"MEMORY", b"PURGE"]), &mut out);
        assert_eq!(out, b"+OK\r\n");
        // MALLOC-STATS — bulk note
        out.clear();
        cmd_memory(&cfg, &store, &argv(&[b"MEMORY", b"MALLOC-STATS"]), &mut out);
        assert!(out.starts_with(b"$"));
        // missing subcommand
        out.clear();
        cmd_memory(&cfg, &store, &argv(&[b"MEMORY"]), &mut out);
        assert!(out.starts_with(b"-ERR wrong number of arguments"));
        // unknown subcommand
        out.clear();
        cmd_memory(&cfg, &store, &argv(&[b"MEMORY", b"NOPE"]), &mut out);
        assert!(out.starts_with(b"-ERR Unknown MEMORY subcommand"));
    }

    #[test]
    fn memory_stats_encodes_all_eight_fields() {
        let cfg = Config::default();
        let mut store = Store::new();
        store.set(b"k1", b"v1".to_vec(), None, false, false);
        store.set(b"k2", b"v2".to_vec(), None, false, false);

        let mut out = Vec::new();
        cmd_memory_stats(&cfg, &store, &mut out);
        // 8 pairs × 2 → *16
        assert!(out.starts_with(b"*16\r\n"));
        // Spot-check the key labels are present.
        for label in [
            b"peak.allocated".as_slice(),
            b"total.allocated".as_slice(),
            b"keys.count".as_slice(),
            b"keys.bytes-per-key".as_slice(),
            b"maxmemory".as_slice(),
            b"maxmemory.policy".as_slice(),
            b"evicted.keys".as_slice(),
            b"entry.overhead".as_slice(),
        ] {
            assert!(
                out.windows(label.len()).any(|w| w == label),
                "missing label {:?} in stats output",
                std::str::from_utf8(label).unwrap()
            );
        }
    }

    #[test]
    fn avg_bytes_per_key_handles_empty_store() {
        let store = Store::new();
        assert_eq!(avg_bytes_per_key(&store), 0);
    }

    #[test]
    fn avg_bytes_per_key_divides_used_by_dbsize() {
        let mut store = Store::new();
        store.set(b"k1", b"v1".to_vec(), None, false, false);
        store.set(b"k2", b"v2".to_vec(), None, false, false);
        let avg = avg_bytes_per_key(&store);
        assert!(avg > 0, "expected avg > 0 with two small keys; got {avg}");
        // Sanity bound: well under the per-entry overhead × 2 ceiling.
        assert!(avg < (ENTRY_OVERHEAD as i64) * 10);
    }
}
