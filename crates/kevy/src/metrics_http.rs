//! v1.41 — Prometheus `/metrics` HTTP exposition endpoint.
//!
//! Std-only tiny HTTP/1.1 server (0-dep, no `hyper`). One background
//! thread per `serve()` call; accepts conns serially (scrapers are
//! polling, low-rate). Emits `text/plain; version=0.0.4` Prometheus
//! exposition format on `GET /metrics`. Anything else returns 404.
//!
//! Metric source: reads the live [`crate::stats::Totals`] + the
//! process-global [`Config`].

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::time::Instant;

use kevy_config::Config;

use crate::ops::stats;

/// Spawn the metrics HTTP listener if `cfg.metrics.listen_port > 0`.
/// Returns immediately; the listener runs on a daemon thread.
pub fn spawn_if_enabled(cfg: &Arc<Config>) {
    let port = cfg.metrics.listen_port;
    if port == 0 {
        return;
    }
    let cfg_clone: Arc<Config> = Arc::clone(cfg);
    std::thread::Builder::new()
        .name("kevy-metrics".into())
        .spawn(move || run_listener(port, cfg_clone))
        .expect("spawn kevy-metrics thread");
}

fn run_listener(port: u16, cfg: Arc<Config>) {
    let addr = format!("127.0.0.1:{port}");
    let listener = match TcpListener::bind(&addr) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("kevy: metrics endpoint failed to bind {addr}: {e}");
            return;
        }
    };
    eprintln!("kevy: metrics endpoint listening on http://{addr}/metrics");
    let start = Instant::now();
    loop {
        let (mut conn, _peer) = match listener.accept() {
            Ok(t) => t,
            Err(_) => continue,
        };
        let _ = conn.set_read_timeout(Some(std::time::Duration::from_secs(2)));
        let _ = conn.set_write_timeout(Some(std::time::Duration::from_secs(2)));
        // Minimal HTTP/1.1 request parse: read up to the request-line
        // + drain to end-of-headers (\r\n\r\n).
        let mut buf = [0u8; 1024];
        let n = match conn.read(&mut buf) {
            Ok(n) => n,
            Err(_) => continue,
        };
        let raw = &buf[..n];
        // We only care about the verb + path; any GET /metrics passes.
        let is_metrics = raw.starts_with(b"GET /metrics");
        let body = if is_metrics {
            render_metrics(&cfg, start.elapsed().as_secs())
        } else {
            String::new()
        };
        let resp = if is_metrics {
            format!(
                "HTTP/1.1 200 OK\r\n\
                 Content-Type: text/plain; version=0.0.4\r\n\
                 Content-Length: {}\r\n\
                 Connection: close\r\n\
                 \r\n{}",
                body.len(),
                body,
            )
        } else {
            "HTTP/1.1 404 Not Found\r\n\
             Content-Length: 0\r\n\
             Connection: close\r\n\
             \r\n"
                .to_string()
        };
        let _ = conn.write_all(resp.as_bytes());
    }
}

/// Produce the Prometheus exposition body. Reads totals from the
/// process-global stats; uses `cfg` for static values like max_clients.
fn render_metrics(cfg: &Arc<Config>, uptime_seconds: u64) -> String {
    let mut out = String::with_capacity(4 * 1024);
    let totals = stats::aggregate();

    // Uptime
    push_help(&mut out, "kevy_uptime_seconds", "Seconds since kevy started");
    push_type(&mut out, "kevy_uptime_seconds", "counter");
    push_value(&mut out, "kevy_uptime_seconds", uptime_seconds);

    // Clients
    push_help(&mut out, "kevy_maxclients", "Configured max client connections");
    push_type(&mut out, "kevy_maxclients", "gauge");
    push_value(&mut out, "kevy_maxclients", cfg.server.max_clients as u64);

    // Memory
    push_help(&mut out, "kevy_used_memory_bytes", "Resident keyspace memory");
    push_type(&mut out, "kevy_used_memory_bytes", "gauge");
    push_value(&mut out, "kevy_used_memory_bytes", totals.used_memory);

    push_help(&mut out, "kevy_used_memory_peak_bytes", "Peak resident keyspace memory");
    push_type(&mut out, "kevy_used_memory_peak_bytes", "gauge");
    push_value(&mut out, "kevy_used_memory_peak_bytes", totals.used_memory_peak);

    push_help(&mut out, "kevy_maxmemory_bytes", "Configured maxmemory ceiling (0 = unlimited)");
    push_type(&mut out, "kevy_maxmemory_bytes", "gauge");
    push_value(&mut out, "kevy_maxmemory_bytes", cfg.memory.maxmemory);

    push_help(&mut out, "kevy_evicted_keys_total", "Keys evicted due to maxmemory");
    push_type(&mut out, "kevy_evicted_keys_total", "counter");
    push_value(&mut out, "kevy_evicted_keys_total", totals.evicted_keys);

    push_help(&mut out, "kevy_expired_keys_total", "Keys expired due to TTL");
    push_type(&mut out, "kevy_expired_keys_total", "counter");
    push_value(&mut out, "kevy_expired_keys_total", totals.expired_keys);

    push_help(&mut out, "kevy_keys_total", "Number of keys across all shards");
    push_type(&mut out, "kevy_keys_total", "gauge");
    push_value(&mut out, "kevy_keys_total", totals.keys);

    push_help(&mut out, "kevy_expires_total", "Number of keys with TTL");
    push_type(&mut out, "kevy_expires_total", "gauge");
    push_value(&mut out, "kevy_expires_total", totals.expires);

    // Build info — useful for ops to see which kevy is running.
    push_help(&mut out, "kevy_build_info", "kevy version (always 1)");
    push_type(&mut out, "kevy_build_info", "gauge");
    out.push_str(&format!(
        "kevy_build_info{{version=\"{}\"}} 1\n",
        env!("CARGO_PKG_VERSION")
    ));

    out
}

fn push_help(out: &mut String, name: &str, help: &str) {
    out.push_str("# HELP ");
    out.push_str(name);
    out.push(' ');
    out.push_str(help);
    out.push('\n');
}
fn push_type(out: &mut String, name: &str, ty: &str) {
    out.push_str("# TYPE ");
    out.push_str(name);
    out.push(' ');
    out.push_str(ty);
    out.push('\n');
}
fn push_value(out: &mut String, name: &str, value: u64) {
    out.push_str(name);
    out.push(' ');
    out.push_str(&value.to_string());
    out.push('\n');
}
