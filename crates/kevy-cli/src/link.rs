//! The connection a kevy-cli tool talks through.
//!
//! A tool needs two things from a server: one request and its reply, and a
//! pre-encoded batch with its replies. Both connections kevy-cli has
//! provide them — [`RespClient`] (plain TCP, what the tools grew up on) and
//! the redis-cli half's connection, which also authenticates, speaks unix
//! sockets and URIs, and selects a database. Tools take `&mut dyn Link`, so
//! `kevy-cli --kevy <tool>` runs them on the same connection every other
//! command uses.

use kevy_resp_client::{Reply, RespClient};
use std::io;

/// A request/reply connection to a RESP server.
///
/// ```no_run
/// // Needs a server on 127.0.0.1:6004.
/// use kevy_cli::link::Link;
/// let mut client = kevy_resp_client::RespClient::connect("127.0.0.1", 6004)?;
/// let link: &mut dyn Link = &mut client;
/// let pong = link.request_borrowed(&[b"PING"])?;
/// assert_eq!(pong, kevy_resp_client::Reply::Simple(b"PONG".to_vec()));
/// # Ok::<(), std::io::Error>(())
/// ```
pub trait Link {
    /// Send one command and read its reply.
    fn request_borrowed(&mut self, argv: &[&[u8]]) -> io::Result<Reply>;
    /// Send `raw` (already-encoded commands) in one write and read `n`
    /// replies, in order.
    fn pipeline_raw(&mut self, raw: &[u8], n: usize) -> io::Result<Vec<Reply>>;
}

impl Link for RespClient {
    fn request_borrowed(&mut self, argv: &[&[u8]]) -> io::Result<Reply> {
        RespClient::request_borrowed(self, argv)
    }

    fn pipeline_raw(&mut self, raw: &[u8], n: usize) -> io::Result<Vec<Reply>> {
        RespClient::pipeline_raw(self, raw, n)
    }
}
