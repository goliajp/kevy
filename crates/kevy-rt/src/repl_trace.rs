//! Temporary replication trace gate for the availgate failover-wedge
//! instrumented-reproduction arc. All probes are compiled in but cost
//! one cached boolean when the env var is absent.
//!
//! Enable with `KEVY_DEBUG_REPL_TRACE=1` (any non-empty value). The
//! probe surface covers the two silent pump arms (fresh-cursor adopt,
//! caught-up early return), the promotion bump moment (pre-bump feed
//! position + attached cursor states), the replication handshake, and
//! snapshot ship begin/end — the sequencing window the wedge lives in.

use std::sync::OnceLock;

static GATE: OnceLock<bool> = OnceLock::new();

/// True when replication tracing is enabled for this process.
pub fn repl_trace() -> bool {
    *GATE.get_or_init(|| std::env::var_os("KEVY_DEBUG_REPL_TRACE").is_some_and(|v| !v.is_empty()))
}

/// Emit one trace line stamped with the shared wall clock (epoch ms) —
/// the availgate crime scene spans three processes on one host, and
/// only a shared clock lets their probe lines interleave into a single
/// sequence. Callers gate on [`repl_trace`] first.
pub fn repl_trace_line(msg: std::fmt::Arguments<'_>) {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    eprintln!("kevy: [repltrace t={ms}] {msg}");
}
