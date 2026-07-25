//! `FAILOVER <host> <port> [TIMEOUT ms]`: the planned,
//! zero-loss primary handover, built ENTIRELY from existing verbs:
//!
//!   1. quiesce writes here (`-QUIESCED migrating to <target>` — the
//!      cluster-rw client already retries with backoff),
//!   2. wait for the target replica to drain (its own INFO reports
//!      `slave_lag_frames:0` with the link up; with writes quiesced,
//!      converged gauges are exact),
//!   3. promote the target (`REPLICAOF NO ONE` over a plain client
//!      connection) and retarget ourselves at it,
//!   4. un-quiesce — we are now the read-only replica, and stray
//!      writes get `-READONLY` / follow the new primary.
//!
//! Asynchronous like Redis's FAILOVER: the verb answers +OK at once
//! and the handover runs on a background thread. `FAILOVER ABORT`
//! clears the quiesce; the thread notices and stands down. Timeout
//! (default 10s) rolls back to normal primary duty.

use std::io::Write as _;
use std::sync::Arc;

use kevy_resp::{ArgvView, encode_error, encode_simple_string};

use crate::state::{Ctx, ReplicationState};

pub(crate) fn cmd_failover<A: ArgvView + ?Sized>(ctx: &Ctx<'_>, args: &A, out: &mut Vec<u8>) {
    let repl = &ctx.state.replication;
    if args.get(1).is_some_and(|a| a.eq_ignore_ascii_case(b"ABORT")) {
        repl.set_quiesce(None);
        encode_simple_string(out, "OK");
        return;
    }
    let (Some(host), Some(port)) = (args.get(1), args.get(2)) else {
        encode_error(out, "ERR FAILOVER host port [TIMEOUT ms] — run COMMAND DOCS FAILOVER");
        return;
    };
    let Ok(host) = std::str::from_utf8(host) else {
        encode_error(out, "ERR FAILOVER: host is not utf-8");
        return;
    };
    let Some(port): Option<u16> = std::str::from_utf8(port).ok().and_then(|p| p.parse().ok())
    else {
        encode_error(out, "ERR FAILOVER: port is not a u16");
        return;
    };
    let timeout_ms: u64 = args
        .get(3)
        .filter(|t| t.eq_ignore_ascii_case(b"TIMEOUT"))
        .and_then(|_| args.get(4))
        .and_then(|v| std::str::from_utf8(v).ok())
        .and_then(|v| v.parse().ok())
        .unwrap_or(10_000);
    if repl.is_replica() {
        encode_error(out, "ERR FAILOVER: this node is a replica — run it on the primary");
        return;
    }
    let target = format!("{host}:{port}");
    repl.set_quiesce(Some(target.clone()));
    let host = host.to_string();
    // The handover thread captures the narrow Arc<ReplicationState>
    // slice, never the whole RuntimeState.
    let repl = Arc::clone(repl);
    std::thread::Builder::new()
        .name("kevy-failover".into())
        .spawn(move || run_handover(&repl, host, port, timeout_ms))
        .ok();
    encode_simple_string(out, "OK");
}

fn run_handover(repl: &ReplicationState, host: String, port: u16, timeout_ms: u64) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    let target = format!("{host}:{port}");
    // Phase 1: wait for the target to drain (lag 0, link up).
    loop {
        if !repl.quiesce_active() {
            eprintln!("kevy: FAILOVER to {target} aborted");
            return;
        }
        if std::time::Instant::now() > deadline {
            eprintln!("kevy: FAILOVER to {target} timed out waiting for drain; resuming primary duty");
            repl.set_quiesce(None);
            return;
        }
        match target_drained(&host, port) {
            Ok(true) => break,
            Ok(false) => std::thread::sleep(std::time::Duration::from_millis(100)),
            Err(e) => {
                eprintln!("kevy: FAILOVER probe of {target} failed ({e}); retrying");
                std::thread::sleep(std::time::Duration::from_millis(250));
            }
        }
    }
    // Phase 2: promote the target, then follow it.
    if let Err(e) = send_verb(&host, port, &[b"REPLICAOF", b"NO", b"ONE"]) {
        eprintln!("kevy: FAILOVER promote of {target} failed ({e}); resuming primary duty");
        repl.set_quiesce(None);
        return;
    }
    let upstream = format!("{host}:{}", port + 10_000);
    match crate::replication::retarget_upstream(repl, &upstream) {
        Ok(()) => {
            repl.set_quiesce(None);
            eprintln!("kevy: FAILOVER complete — now replicating from {target}");
        }
        Err(e) => {
            // The target IS promoted; staying quiesced would strand
            // writes with a dangling pointer. Surface loudly.
            repl.set_quiesce(None);
            eprintln!(
                "kevy: FAILOVER promoted {target} but local retarget failed ({e}) — \
                 run REPLICAOF {host} {port} manually"
            );
        }
    }
}

/// One INFO probe: is the target's replication link up with zero lag?
fn target_drained(host: &str, port: u16) -> std::io::Result<bool> {
    let mut c = kevy_resp_client::RespClient::connect(host, port)?;
    let reply = c.request_borrowed(&[b"INFO", b"replication"])?;
    let kevy_resp::Reply::Bulk(body) = reply else {
        return Ok(false);
    };
    let text = String::from_utf8_lossy(&body);
    Ok(text.contains("master_link_status:up") && text.contains("slave_lag_frames:0"))
}

fn send_verb(host: &str, port: u16, argv: &[&[u8]]) -> std::io::Result<()> {
    let mut c = kevy_resp_client::RespClient::connect(host, port)?;
    let _ = c.request_borrowed(argv)?;
    let _ = std::io::stderr().flush();
    Ok(())
}
