//! Inbound event handling: socket-readable, cross-core ring drain, and
//! connection teardown. The event loop (`run`), transport setup
//! (accept_ready, flush_conn, flush_dirty, maybe_auto_rewrite_aof), and
//! cross-shard send/backlog plumbing live in [`crate::shard`]; the
//! *semantics* (routing, execution, reduction) live in [`crate::exec`].
//! Split out so each file stays under the 500-LOC house rule without
//! breaking the established two-impl-block layering.

use std::io;

use kevy_resp::parse_command_into;

use crate::Commands;
use crate::message::{Inbound, Op};
use crate::shard::Shard;

impl<C: Commands> Shard<C> {
    /// Socket readable: read until WouldBlock, then parse out every full
    /// RESP command into `scratch_argv` and dispatch it. Zero-alloc hot
    /// path; the in-cmd prefetch is issued before dispatch on the key (if
    /// any) so the bucket line moves toward L1 while start_command /
    /// handle_command's prologue runs.
    pub(crate) fn conn_readable(&mut self, conn_id: u64) -> io::Result<()> {
        {
            let Some(conn) = self.conns.get_mut(&conn_id) else {
                return Ok(());
            };
            loop {
                match conn.sock.read(&mut self.read_buf) {
                    Ok(0) => {
                        conn.closing = true;
                        break;
                    }
                    Ok(n) => conn.input.extend_from_slice(&self.read_buf[..n]),
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(_) => {
                        conn.closing = true;
                        break;
                    }
                }
            }
        }

        // Parse + dispatch via mem::replace so handle_command can take
        // &mut self while we hold the parsed argv on the stack —
        // `self.scratch_argv` is temporarily a default empty Argv during
        // dispatch, restored after to keep buf+ends capacity warm.
        let mut had_protocol_error = false;
        loop {
            let consumed = {
                let Some(conn) = self.conns.get_mut(&conn_id) else {
                    return Ok(());
                };
                match parse_command_into(&conn.input, &mut self.scratch_argv) {
                    Ok(Some(c)) => Some(c),
                    Ok(None) => None,
                    Err(_) => {
                        had_protocol_error = true;
                        None
                    }
                }
            };
            match consumed {
                Some(c) => {
                    if let Some(conn) = self.conns.get_mut(&conn_id) {
                        conn.input.drain(..c);
                    } else {
                        return Ok(());
                    }
                    let argv = std::mem::take(&mut self.scratch_argv);
                    if let Some(key) = argv.get(1) {
                        self.store.prefetch_for_key(key);
                    }
                    self.handle_command(conn_id, &argv);
                    self.scratch_argv = argv;
                }
                None => break,
            }
        }
        if had_protocol_error {
            self.protocol_error(conn_id);
        }
        self.flush_conn(conn_id)
    }

    /// Drain inbound cross-core messages from every peer ring; returns
    /// whether any were processed.
    pub(crate) fn drain_inbound(&mut self) -> io::Result<bool> {
        let mut did = false;
        for src in 0..self.nshards {
            if src == self.id {
                continue; // no self-ring
            }
            while let Some(msg) = self.inboxes[src].as_mut().expect("peer inbox").pop() {
                did = true;
                match msg {
                    Inbound::Request {
                        origin,
                        conn,
                        seq,
                        op,
                    } => {
                        let part = self.exec_op(op);
                        self.send_to(origin, Inbound::Response { conn, seq, part });
                    }
                    Inbound::Response { conn, seq, part } => {
                        self.fold(conn, seq, part);
                        self.flush_conn(conn)?;
                    }
                    // Batched single-key dispatches to this (owning) shard:
                    // exec each locally, reply as one `ResponseBatch` to the
                    // origin.
                    Inbound::RequestBatch { origin, reqs } => {
                        let mut resps = Vec::with_capacity(reqs.len());
                        for (conn, seq, argv) in reqs {
                            let part = self.exec_op(Op::Dispatch(argv));
                            resps.push((conn, seq, part));
                        }
                        self.send_to(origin, Inbound::ResponseBatch(resps));
                    }
                    // Batched replies: fold each by seq, then flush each
                    // touched conn once (dedup — pipelined replies share a
                    // conn).
                    Inbound::ResponseBatch(resps) => {
                        let mut to_flush: Vec<u64> = Vec::new();
                        for (conn, seq, part) in resps {
                            self.fold(conn, seq, part);
                            if !to_flush.contains(&conn) {
                                to_flush.push(conn);
                            }
                        }
                        for conn in to_flush {
                            self.flush_conn(conn)?;
                        }
                    }
                    // Fire-and-forget batched pub/sub delivery; appended
                    // subscriber output is flushed via `flush_dirty`.
                    Inbound::DeliverPublish(batch) => {
                        for m in &batch {
                            self.deliver_publish(&m.0, &m.1);
                        }
                    }
                }
            }
        }
        Ok(did)
    }

    /// Tear down a closing connection: deregister from the poller, drop
    /// its subscriptions from the pub/sub registry, and release its
    /// `Socket` (closing the fd).
    pub(crate) fn close_conn(&mut self, conn_id: u64) {
        if let Some(conn) = self.conns.remove(&conn_id) {
            let fd = conn.sock.raw();
            let _ = self.poller.delete(fd);
            self.fd_to_conn.remove(&fd);
            self.unregister_subs(&conn.sub);
            // conn (and its Socket) dropped here → fd closed.
        }
    }
}
