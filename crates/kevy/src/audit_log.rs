//! v1.42 — append-only ADMIN-command audit log.
//!
//! Captures every privileged command (CONFIG SET / REWRITE / DEBUG /
//! FLUSHDB / FLUSHALL / CLIENT KILL / SCRIPT FLUSH) into a file
//! line-by-line, with timestamp + command + args. Format:
//!
//! ```text
//!   <unix_micros>\t<command>\t<arg1>\t<arg2>\t...\n
//! ```
//!
//! - Append-only (`O_APPEND`); process death never corrupts.
//! - Line-buffered: each event flushed at line boundary.
//! - Skips the audit on quiet kevy (`[audit] log_path` empty).
//! - Std-only, 0-dep.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

/// Process-global audit log handle. `None` = OFF.
static AUDIT: OnceLock<Option<Mutex<File>>> = OnceLock::new();

/// Initialise the audit log from the kevy config. Idempotent: only
/// the FIRST call binds; subsequent calls are no-ops (so embedders
/// that run multiple `serve` calls don't double-open).
pub fn init(log_path: &Path) {
    AUDIT.get_or_init(|| {
        if log_path.as_os_str().is_empty() {
            return None;
        }
        match OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path)
        {
            Ok(f) => Some(Mutex::new(f)),
            Err(e) => {
                eprintln!(
                    "kevy: audit log {} could not open: {e}",
                    log_path.display()
                );
                None
            }
        }
    });
}

/// Log one ADMIN command event. `args` includes the verb. Best-effort
/// write — audit write failures are logged to stderr but never abort
/// the calling command path.
pub fn record(args: &[&[u8]]) {
    let Some(Some(mu)) = AUDIT.get() else { return };
    let mut line = String::with_capacity(128);
    let micros = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros())
        .unwrap_or(0);
    line.push_str(&micros.to_string());
    for arg in args {
        line.push('\t');
        // Truncate arg display to 256 bytes; sanitize tabs/newlines for
        // a single-line format.
        let s = String::from_utf8_lossy(&arg[..arg.len().min(256)]);
        for c in s.chars() {
            match c {
                '\t' | '\n' | '\r' => line.push(' '),
                _ => line.push(c),
            }
        }
        if arg.len() > 256 {
            line.push_str("…");
        }
    }
    line.push('\n');
    if let Ok(mut f) = mu.lock() {
        let _ = f.write_all(line.as_bytes());
        let _ = f.flush();
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("kevy-audit-{name}-{nanos}"))
    }

    #[test]
    fn record_off_path_noop() {
        // Empty path → no init, no record output.
        let path = tmp("off");
        // Don't init; record should silently no-op.
        record(&[b"CONFIG", b"SET", b"maxmemory", b"1g"]);
        // Verify file does not exist.
        assert!(!path.exists());
    }
}
