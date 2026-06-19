//! async-std runtime adapter — implements [`crate::transport::AsyncRead`]
//! / [`crate::transport::AsyncWrite`] on top of `async_std::net::TcpStream`.
//!
//! Activated by the `async-std` Cargo feature.
//!
//! async-std uses the futures-io trait shape (`&mut [u8]` slot,
//! `Poll<io::Result<usize>>`), identical to ours, so this adapter is
//! pure forwarding — same shape as the smol adapter.

use core::pin::Pin;
use core::task::{Context, Poll};
use std::io;

use async_std::io::{Read as AsyncStdRead, Write as AsyncStdWrite};
use async_std::net::TcpStream;

use crate::transport::{AsyncRead, AsyncWrite};

impl AsyncRead for TcpStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        <Self as AsyncStdRead>::poll_read(self, cx, buf)
    }
}

impl AsyncWrite for TcpStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        <Self as AsyncStdWrite>::poll_write(self, cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        <Self as AsyncStdWrite>::poll_flush(self, cx)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        <Self as AsyncStdWrite>::poll_close(self, cx)
    }
}

/// Connect an async-std `TcpStream` to `host:port`, enabling
/// `TCP_NODELAY` best-effort. Mirrors [`crate::rt_tokio::connect`].
pub async fn connect(host: &str, port: u16) -> io::Result<TcpStream> {
    let stream = TcpStream::connect((host, port)).await?;
    stream.set_nodelay(true).ok();
    Ok(stream)
}
