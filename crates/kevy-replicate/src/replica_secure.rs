//! The replica side of a Noise-protected replication link: the handshake,
//! then sealed writes and decrypted reads under the plaintext protocol.

use std::io::{self, Read, Write};
use std::net::TcpStream;

use kevy_noise::{Frames, Initiator, Keypair, MAX_MESSAGE, Transport, frame};

use crate::replica::{ReplicaClient, connect_stream, encode_replicate_from, parse_ack_line};
use crate::replica_error::ReplicaError;

/// Must match the primary's.
const PROLOGUE: &[u8] = b"kevy-replicate\x001";
const TAG: usize = 16;

/// What a replica needs to open a secure link: its own key, and the
/// primary's public key it will accept.
///
/// ```
/// use kevy_noise::Keypair;
/// use kevy_replicate::replica::ReplicaSecurity;
///
/// let sec = ReplicaSecurity { local: Keypair::from_secret([2; 32]), primary_key: [9; 32] };
/// assert_eq!(sec.primary_key, [9; 32]);
/// ```
#[derive(Debug, Clone)]
pub struct ReplicaSecurity {
    /// The replica's static key pair; a primary with `replica_keys` set
    /// lists its public half.
    ///
    /// ```
    /// let sec = kevy_replicate::replica::ReplicaSecurity { local: kevy_noise::Keypair::from_secret([2; 32]), primary_key: [9; 32] };
    /// assert_ne!(sec.local.public(), [0; 32]);
    /// ```
    pub local: Keypair,
    /// The primary's public key; a primary holding any other key cannot
    /// complete the handshake.
    ///
    /// ```
    /// let primary = kevy_noise::Keypair::from_secret([1; 32]);
    /// let sec = kevy_replicate::replica::ReplicaSecurity {
    ///     local: kevy_noise::Keypair::from_secret([2; 32]),
    ///     primary_key: primary.public(),
    /// };
    /// assert_eq!(sec.primary_key, primary.public());
    /// ```
    pub primary_key: [u8; 32],
}

pub(crate) struct ClientNoise {
    transport: Transport,
    frames: Frames,
}

impl core::fmt::Debug for ClientNoise {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("ClientNoise")
    }
}

fn invalid(e: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e.to_string())
}

#[cfg(not(target_arch = "wasm32"))]
fn fresh_secret() -> io::Result<[u8; 32]> {
    let mut secret = [0u8; 32];
    kevy_sys::fill_random(&mut secret)?;
    Ok(secret)
}

// no entropy source, and no TCP replication to protect, on wasm32
#[cfg(target_arch = "wasm32")]
fn fresh_secret() -> io::Result<[u8; 32]> {
    Err(io::ErrorKind::Unsupported.into())
}

pub(crate) fn handshake(
    sock: &mut TcpStream,
    sec: &ReplicaSecurity,
) -> Result<ClientNoise, ReplicaError> {
    let ephemeral = Keypair::from_secret(fresh_secret()?);
    let (m1, init) = Initiator::start(&sec.local, &sec.primary_key, ephemeral, PROLOGUE, b"")
        .map_err(invalid)?;
    sock.write_all(&frame(&m1).map_err(invalid)?)?;
    let mut frames = Frames::default();
    let mut chunk = [0u8; 256];
    let reply = loop {
        if let Some(m) = frames.next() {
            break m;
        }
        let n = sock.read(&mut chunk)?;
        if n == 0 {
            return Err(ReplicaError::HandshakeRejected);
        }
        frames.push(&chunk[..n]);
    };
    // the primary answers only if it accepted our key; a wrong primary key
    // fails here
    let (_, transport) = init.finish(&reply).map_err(invalid)?;
    Ok(ClientNoise { transport, frames })
}

impl ClientNoise {
    pub(crate) fn write(&mut self, sock: &mut TcpStream, plain: &[u8]) -> io::Result<()> {
        let mut out = Vec::with_capacity(plain.len() + 2 * TAG + 4);
        for chunk in plain.chunks(MAX_MESSAGE - TAG) {
            let sealed = self.transport.seal(chunk).map_err(invalid)?;
            out.extend(frame(&sealed).map_err(invalid)?);
        }
        sock.write_all(&out)
    }

    /// One socket read; the plaintext of every frame it completes is
    /// appended to `out`. Returns the raw byte count, `0` at end of stream.
    pub(crate) fn read(
        &mut self,
        sock: &mut TcpStream,
        chunk: &mut [u8],
        out: &mut Vec<u8>,
    ) -> io::Result<usize> {
        let n = sock.read(chunk)?;
        self.frames.push(&chunk[..n]);
        while let Some(m) = self.frames.next() {
            out.extend(self.transport.open(&m).map_err(invalid)?);
        }
        Ok(n)
    }
}

impl ReplicaClient {
    /// [`Self::connect_at`] over a Noise IK link: the primary must present
    /// `security.primary_key`, and it sees this replica's key before it
    /// answers. Everything after the handshake is encrypted.
    pub fn connect_secure<A: std::net::ToSocketAddrs>(
        addr: A,
        replica_id: &str,
        generation: u64,
        from_offset: u64,
        connect_timeout: std::time::Duration,
        security: &ReplicaSecurity,
    ) -> Result<Self, ReplicaError> {
        let mut sock = connect_stream(addr, connect_timeout)?;
        sock.set_read_timeout(Some(connect_timeout))?;
        let mut noise = handshake(&mut sock, security)?;
        noise.write(&mut sock, &encode_replicate_from(generation, from_offset, replica_id))?;
        let mut plain = Vec::new();
        let mut chunk = [0u8; 256];
        let line_end = loop {
            if let Some(i) = plain.windows(2).position(|w| w == b"\r\n") {
                break i + 2;
            }
            if plain.len() > 256 {
                return Err(ReplicaError::AckMalformed);
            }
            if noise.read(&mut sock, &mut chunk, &mut plain)? == 0 {
                return Err(ReplicaError::HandshakeRejected);
            }
        };
        let (primary_gen, primary_offset) = parse_ack_line(&plain[..line_end])?;
        sock.set_read_timeout(None)?;
        sock.set_nonblocking(false)?;
        let mut buf = Vec::with_capacity(8 * 1024);
        // frames that arrived in the same message as the +ACK
        buf.extend_from_slice(&plain[line_end..]);
        Ok(ReplicaClient {
            sock,
            buf,
            cursor: 0,
            primary_offset_at_handshake: primary_offset,
            primary_gen_at_handshake: primary_gen,
            expected_offset: from_offset,
            in_snapshot: false,
            noise: Some(noise),
        })
    }
}
