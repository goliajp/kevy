//! Stand-in for the Noise link when the `secure` feature is off: a type
//! with no values, so a client's `noise` is always `None` and the
//! encrypted read and write paths compile to nothing.

use std::io;
use std::net::TcpStream;

#[derive(Debug)]
pub(crate) enum ClientNoise {}

impl ClientNoise {
    pub(crate) fn write(&mut self, _sock: &mut TcpStream, _plain: &[u8]) -> io::Result<()> {
        match *self {}
    }

    pub(crate) fn read(
        &mut self,
        _sock: &mut TcpStream,
        _chunk: &mut [u8],
        _out: &mut Vec<u8>,
    ) -> io::Result<usize> {
        match *self {}
    }
}
