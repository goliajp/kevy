//! v2.9 — optional READ-ONLY RESP listener (RFC D2/D3): expose a
//! live embedded store to external RESP clients (redis-cli, ops
//! tooling) over a whitelist of read ops. One thread per connection
//! (ops-tooling cardinality); zero tax when not enabled — no thread,
//! no socket. Everything outside the whitelist answers
//! `-ERR READONLY embedded listener`.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};

use crate::store::WeakStore;

mod verbs;

/// crate-internal re-export for [`crate::Store::dispatch_readonly`].
pub(crate) use verbs::dispatch as verbs_dispatch;

/// Spawn the accept loop. Holds only a [`WeakStore`] — the listener
/// never keeps the store alive; conns error out after the store
/// drops.
pub(crate) fn spawn(addr: SocketAddr, store: WeakStore) -> std::io::Result<SocketAddr> {
    let sock = TcpListener::bind(addr)?;
    let local = sock.local_addr()?;
    std::thread::Builder::new()
        .name("kevy-resp-listener".into())
        .spawn(move || {
            for conn in sock.incoming() {
                let Ok(conn) = conn else { continue };
                if store.upgrade().is_none() {
                    return; // store gone — stop accepting
                }
                let store = store.clone();
                let _ = std::thread::Builder::new()
                    .name("kevy-resp-conn".into())
                    .spawn(move || serve_conn(conn, &store));
            }
        })?;
    Ok(local)
}

fn serve_conn(mut conn: TcpStream, store: &WeakStore) {
    let _ = conn.set_nodelay(true);
    let mut buf: Vec<u8> = Vec::with_capacity(4096);
    let mut out: Vec<u8> = Vec::with_capacity(4096);
    loop {
        // parse as many complete requests as buffered
        let mut consumed = 0usize;
        out.clear();
        while let Some((argv, used)) = parse_argv(&buf[consumed..]) {
            consumed += used;
            let Some(store) = store.upgrade() else {
                out.extend_from_slice(b"-ERR store closed\r\n");
                let _ = conn.write_all(&out);
                return;
            };
            verbs::dispatch(&store, &argv, &mut out);
        }
        buf.drain(..consumed);
        if !out.is_empty() && conn.write_all(&out).is_err() {
            return;
        }
        let mut chunk = [0u8; 16384];
        match conn.read(&mut chunk) {
            Ok(0) | Err(_) => return,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
        if buf.len() > 1 << 20 {
            return; // oversized pipeline — drop the conn
        }
    }
}

/// Parse one complete RESP array-of-bulks request. Returns the argv +
/// bytes consumed, or `None` if incomplete. Malformed input is
/// answered by dropping the conn (returning a poisoned frame).
fn parse_argv(b: &[u8]) -> Option<(Vec<Vec<u8>>, usize)> {
    if b.is_empty() {
        return None;
    }
    let mut pos = 0usize;
    let line = read_line(b, &mut pos)?;
    if line.first() != Some(&b'*') {
        // inline command (redis-cli PING style): split on whitespace
        let argv: Vec<Vec<u8>> = line
            .split(|&c| c == b' ')
            .filter(|w| !w.is_empty())
            .map(<[u8]>::to_vec)
            .collect();
        return (!argv.is_empty()).then_some((argv, pos));
    }
    let n: usize = std::str::from_utf8(&line[1..]).ok()?.trim().parse().ok()?;
    let mut argv = Vec::with_capacity(n.min(64));
    for _ in 0..n {
        let hdr = read_line(b, &mut pos)?;
        if hdr.first() != Some(&b'$') {
            return None;
        }
        let len: usize = std::str::from_utf8(&hdr[1..]).ok()?.trim().parse().ok()?;
        if b.len() < pos + len + 2 {
            return None;
        }
        argv.push(b[pos..pos + len].to_vec());
        pos += len + 2;
    }
    Some((argv, pos))
}

fn read_line<'b>(b: &'b [u8], pos: &mut usize) -> Option<&'b [u8]> {
    let rest = &b[*pos..];
    let idx = rest.windows(2).position(|w| w == b"\r\n")?;
    let line = &rest[..idx];
    *pos += idx + 2;
    Some(line)
}
