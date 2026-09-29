//! The embed writer's side of one replication connection, plaintext or
//! Noise IK. The protocol above it is the same either way: read the
//! `REPLICATE FROM` request, then write frames.

use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::sync::{Arc, Mutex};

use kevy_noise::{Frames, Keypair, MAX_MESSAGE, Responder, Transport, frame};
use kevy_replicate::feed::FeedPosition;
use kevy_replicate::replica::{ReplicaClient, ReplicaError, ReplicaSecurity};

use crate::config_secure::LinkKeys;

/// Must match the replica's; the server primary uses the same one, so
/// server replicas and embed replicas can follow either kind of primary.
const PROLOGUE: &[u8] = b"kevy-replicate\x001";
const TAG: usize = 16;

pub(crate) enum Wire {
    Plain(TcpStream),
    Noise { sock: TcpStream, transport: Box<Transport>, frames: Frames },
}

fn invalid(e: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e.to_string())
}

impl Wire {
    /// Plaintext when `keys` is `None`; otherwise run the responder
    /// handshake, refusing a replica whose key is not listed.
    pub(crate) fn accept(mut sock: TcpStream, keys: Option<&LinkKeys>) -> io::Result<Self> {
        let Some(keys) = keys else { return Ok(Wire::Plain(sock)) };
        let mut frames = Frames::default();
        let mut chunk = [0u8; 256];
        let first = loop {
            if let Some(m) = frames.next() {
                break m;
            }
            let n = sock.read(&mut chunk)?;
            if n == 0 {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            frames.push(&chunk[..n]);
        };
        let mut secret = [0u8; 32];
        kevy_sys::fill_random(&mut secret)?;
        let (_, resp) =
            Responder::accept(&keys.local, Keypair::from_secret(secret), PROLOGUE, &first)
                .map_err(invalid)?;
        let key = resp.remote_static();
        if !keys.peers.is_empty() && !keys.peers.iter().any(|k| kevy_crypto::ct_eq(k, &key)) {
            return Err(invalid("replica key not listed"));
        }
        let (reply, transport) = resp.finish(b"").map_err(invalid)?;
        sock.write_all(&frame(&reply).map_err(invalid)?)?;
        Ok(Wire::Noise { sock, transport: Box::new(transport), frames })
    }

    /// One socket read; the plaintext it completes is appended to `out`.
    /// Returns the raw byte count, `0` at end of stream.
    pub(crate) fn read_into(&mut self, out: &mut Vec<u8>) -> io::Result<usize> {
        let mut chunk = [0u8; 256];
        match self {
            Wire::Plain(sock) => {
                let n = sock.read(&mut chunk)?;
                out.extend_from_slice(&chunk[..n]);
                Ok(n)
            }
            Wire::Noise { sock, transport, frames } => {
                let n = sock.read(&mut chunk)?;
                frames.push(&chunk[..n]);
                while let Some(m) = frames.next() {
                    out.extend(transport.open(&m).map_err(invalid)?);
                }
                Ok(n)
            }
        }
    }

    pub(crate) fn write_all(&mut self, plain: &[u8]) -> io::Result<()> {
        match self {
            Wire::Plain(sock) => sock.write_all(plain),
            Wire::Noise { sock, transport, .. } => {
                let mut out = Vec::with_capacity(plain.len() + 2 * TAG + 4);
                for chunk in plain.chunks(MAX_MESSAGE - TAG) {
                    out.extend(frame(&transport.seal(chunk).map_err(invalid)?).map_err(invalid)?);
                }
                sock.write_all(&out)
            }
        }
    }

    pub(crate) fn socket(&self) -> &TcpStream {
        match self {
            Wire::Plain(sock) | Wire::Noise { sock, .. } => sock,
        }
    }

    pub(crate) fn shutdown(&self) -> io::Result<()> {
        self.socket().shutdown(Shutdown::Both)
    }
}

/// How a replica reaches its primary: the live upstream (retargeted by
/// `set_upstream`), its id, and the keys when the link is secure.
pub(crate) struct Dialer {
    upstream: Arc<Mutex<String>>,
    replica_id: String,
    keys: Option<LinkKeys>,
    last_good: usize,
}

impl Dialer {
    pub(crate) fn new(
        upstream: Arc<Mutex<String>>,
        replica_id: String,
        keys: Option<LinkKeys>,
    ) -> Self {
        Self { upstream, replica_id, keys, last_good: 0 }
    }

    /// One connect attempt, presenting the data's `generation` and
    /// resuming at `from_offset`.
    pub(crate) fn dial(
        &mut self,
        generation: u64,
        from_offset: u64,
    ) -> Result<ReplicaClient, ReplicaError> {
        let target =
            self.upstream.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
        match &self.keys {
            Some(keys) => connect_trusted(
                &target,
                &self.replica_id,
                generation,
                from_offset,
                keys,
                &mut self.last_good,
            ),
            None => ReplicaClient::connect_with(
                &target,
                &kevy_replicate::replica::ConnectOptions::new(&self.replica_id)
                    .with_from(FeedPosition::new(generation, from_offset)),
            ),
        }
    }
}

/// Connect to `target` over Noise, trying each trusted primary key in
/// turn from `last_good`, the one that answered last time; a stable
/// primary costs one handshake, and a failover to another trusted node
/// needs no new configuration.
fn connect_trusted(
    target: &str,
    replica_id: &str,
    generation: u64,
    from_offset: u64,
    keys: &LinkKeys,
    last_good: &mut usize,
) -> Result<ReplicaClient, ReplicaError> {
    let n = keys.peers.len();
    let mut last_err = ReplicaError::HandshakeRejected;
    for i in 0..n {
        let k = (*last_good + i) % n;
        let sec = ReplicaSecurity::new(keys.local.clone(), keys.peers[k]);
        match ReplicaClient::connect_with(
            target,
            &kevy_replicate::replica::ConnectOptions::new(replica_id)
                .with_from(FeedPosition::new(generation, from_offset))
                .with_security(sec),
        ) {
            Ok(c) => {
                *last_good = k;
                return Ok(c);
            }
            Err(e) => last_err = e,
        }
    }
    Err(last_err)
}
