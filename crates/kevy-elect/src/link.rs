//! One election connection, plain or Noise IK. A secure link authenticates
//! the peer by its static key before any election message is read, so the
//! sender of every message after it is known rather than claimed.

use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use kevy_noise::{Frames, Initiator, Keypair, Responder, Transport, frame};

const PROLOGUE: &[u8] = b"kevy-elect\x001";
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(2);

/// This node's key and every peer's, for [`crate::transport::Transport::spawn_secure`].
///
/// ```
/// use kevy_elect::SecureLinks;
/// use kevy_noise::Keypair;
///
/// let links = SecureLinks::new(Keypair::from_secret([1; 32]), [("n2".to_string(), [2; 32])]);
/// assert_eq!(links.peer_keys.len(), 1);
/// ```
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct SecureLinks {
    /// This node's static key pair.
    ///
    /// ```
    /// let links = kevy_elect::SecureLinks::new(kevy_noise::Keypair::from_secret([1; 32]), []);
    /// assert_ne!(links.local.public(), [0; 32]);
    /// ```
    pub local: Keypair,
    /// `(node_id, public key)` for every peer. A connection presenting any
    /// other key is dropped.
    ///
    /// ```
    /// let n2 = kevy_noise::Keypair::from_secret([2; 32]);
    /// let links = kevy_elect::SecureLinks::new(
    ///     kevy_noise::Keypair::from_secret([1; 32]),
    ///     [("n2".to_string(), n2.public())],
    /// );
    /// assert_eq!(links.peer_keys[0].0, "n2");
    /// ```
    pub peer_keys: Vec<(String, [u8; 32])>,
}

impl SecureLinks {
    /// This node's key pair `local` and the public key of every peer, by
    /// node id. Neither has a default: a secure link without them
    /// authenticates nobody.
    ///
    /// ```
    /// let links = kevy_elect::SecureLinks::new(
    ///     kevy_noise::Keypair::from_secret([1; 32]),
    ///     [("n2".to_string(), [2; 32]), ("n3".to_string(), [3; 32])],
    /// );
    /// assert_eq!(links.peer_keys[1].0, "n3");
    /// ```
    pub fn new(local: Keypair, peer_keys: impl IntoIterator<Item = (String, [u8; 32])>) -> Self {
        Self { local, peer_keys: peer_keys.into_iter().collect() }
    }
}

pub(crate) enum Link {
    Plain(TcpStream),
    Noise(Box<NoiseLink>),
}

pub(crate) struct NoiseLink {
    stream: TcpStream,
    transport: Transport,
    frames: Frames,
}

fn noise_err(e: kevy_noise::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e)
}

fn ephemeral() -> io::Result<Keypair> {
    let mut secret = [0u8; 32];
    kevy_sys::fill_random(&mut secret)?;
    Ok(Keypair::from_secret(secret))
}

/// Block until one whole frame has arrived.
fn read_frame(stream: &mut TcpStream, frames: &mut Frames) -> io::Result<Vec<u8>> {
    let mut chunk = [0u8; 1024];
    loop {
        if let Some(msg) = frames.next() {
            return Ok(msg);
        }
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        frames.push(&chunk[..n]);
    }
}

/// Dial side: prove who we are to `peer_id`, and check it is who its
/// configured key says.
pub(crate) fn initiate(
    mut stream: TcpStream,
    secure: &SecureLinks,
    peer_id: &str,
) -> io::Result<Link> {
    let remote =
        secure.peer_keys.iter().find(|(id, _)| id == peer_id).map(|(_, k)| *k).ok_or_else(
            || {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("no key configured for peer {peer_id}"),
                )
            },
        )?;
    stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
    let (m1, init) =
        Initiator::start(&secure.local, &remote, ephemeral()?, PROLOGUE, b"").map_err(noise_err)?;
    stream.write_all(&frame(&m1).map_err(noise_err)?)?;
    let mut frames = Frames::default();
    let m2 = read_frame(&mut stream, &mut frames)?;
    let (_, transport) = init.finish(&m2).map_err(noise_err)?;
    Ok(Link::Noise(Box::new(NoiseLink { stream, transport, frames })))
}

/// Accept side: the peer's node id, established by its key.
pub(crate) fn respond(mut stream: TcpStream, secure: &SecureLinks) -> io::Result<(Link, String)> {
    stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
    let mut frames = Frames::default();
    let m1 = read_frame(&mut stream, &mut frames)?;
    let (_, resp) =
        Responder::accept(&secure.local, ephemeral()?, PROLOGUE, &m1).map_err(noise_err)?;
    let key = resp.remote_static();
    let peer = secure
        .peer_keys
        .iter()
        .find(|(_, k)| kevy_crypto::ct_eq(k, &key))
        .map(|(id, _)| id.clone());
    let peer = peer.ok_or_else(|| {
        io::Error::new(io::ErrorKind::PermissionDenied, "election peer key not configured")
    })?;
    let (m2, transport) = resp.finish(b"").map_err(noise_err)?;
    stream.write_all(&frame(&m2).map_err(noise_err)?)?;
    Ok((Link::Noise(Box::new(NoiseLink { stream, transport, frames })), peer))
}

impl Link {
    pub(crate) fn stream(&self) -> &TcpStream {
        match self {
            Link::Plain(s) => s,
            Link::Noise(n) => &n.stream,
        }
    }

    pub(crate) fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        match self {
            Link::Plain(s) => s.write_all(bytes),
            Link::Noise(n) => {
                let sealed = n.transport.seal(bytes).map_err(noise_err)?;
                n.stream.write_all(&frame(&sealed).map_err(noise_err)?)
            }
        }
    }

    /// Read what the socket has and append the plaintext to `out`.
    /// `Ok(0)` is end of stream.
    pub(crate) fn read_into(&mut self, chunk: &mut [u8], out: &mut Vec<u8>) -> io::Result<usize> {
        match self {
            Link::Plain(s) => {
                let n = s.read(chunk)?;
                out.extend_from_slice(&chunk[..n]);
                Ok(n)
            }
            Link::Noise(link) => {
                let n = link.stream.read(chunk)?;
                if n == 0 {
                    return Ok(0);
                }
                link.frames.push(&chunk[..n]);
                while let Some(msg) = link.frames.next() {
                    out.extend(link.transport.open(&msg).map_err(noise_err)?);
                }
                Ok(n)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn dialing_a_peer_without_a_configured_key_fails_before_sending() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let stream = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (mut accepted, _) = listener.accept().unwrap();
        let secure =
            SecureLinks::new(kevy_noise::Keypair::from_secret([1; 32]), [("n2".into(), [2; 32])]);
        let err = initiate(stream, &secure, "n3").err().unwrap();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(err.to_string().contains("n3"));
        // nothing reached the wire: the dropped stream reads as a clean close
        let mut buf = [0u8; 1];
        assert_eq!(accepted.read(&mut buf).unwrap(), 0);
    }
}
