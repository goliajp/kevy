//! After the handshake: one cipher per direction, and the length-prefixed
//! framing that carries each Noise message over a byte stream.

use crate::Error;
use crate::symmetric::CipherState;

/// The largest Noise message: 65,535 bytes, tag included.
///
/// ```
/// assert_eq!(kevy_noise::MAX_MESSAGE, u16::MAX as usize);
/// ```
pub const MAX_MESSAGE: usize = 65_535;
const TAG: usize = 16;

/// The two directions of an established session.
///
/// ```
/// use kevy_noise::{Initiator, Keypair, Responder};
///
/// let (server, client) = (Keypair::from_secret([1; 32]), Keypair::from_secret([2; 32]));
/// let (m1, init) = Initiator::start(&client, &server.public(), Keypair::from_secret([3; 32]), b"", b"").unwrap();
/// let (_, r) = Responder::accept(&server, Keypair::from_secret([4; 32]), b"", &m1).unwrap();
/// let (m2, mut s) = r.finish(b"").unwrap();
/// let (_, mut c) = init.finish(&m2).unwrap();
/// assert_eq!(c.handshake_hash(), s.handshake_hash());
/// ```
pub struct Transport {
    send: CipherState,
    recv: CipherState,
    hash: [u8; 32],
}

impl Transport {
    pub(crate) fn new(send: CipherState, recv: CipherState, hash: [u8; 32]) -> Self {
        Transport { send, recv, hash }
    }

    /// Encrypt one message for the peer; at most `MAX_MESSAGE - 16` bytes.
    ///
    /// ```
    /// # use kevy_noise::{Initiator, Keypair, Responder};
    /// # let (server, client) = (Keypair::from_secret([1; 32]), Keypair::from_secret([2; 32]));
    /// # let (m1, init) = Initiator::start(&client, &server.public(), Keypair::from_secret([3; 32]), b"", b"").unwrap();
    /// # let (_, r) = Responder::accept(&server, Keypair::from_secret([4; 32]), b"", &m1).unwrap();
    /// # let (m2, _) = r.finish(b"").unwrap();
    /// # let (_, mut c) = init.finish(&m2).unwrap();
    /// assert_eq!(c.seal(b"abc").unwrap().len(), 3 + 16);
    /// assert!(c.seal(&vec![0; kevy_noise::MAX_MESSAGE]).is_err());
    /// ```
    pub fn seal(&mut self, plaintext: &[u8]) -> Result<Vec<u8>, Error> {
        if plaintext.len() > MAX_MESSAGE - TAG {
            return Err(Error::TooLong);
        }
        let mut buf = plaintext.to_vec();
        self.send.encrypt(&[], &mut buf)?;
        Ok(buf)
    }

    /// Decrypt one message from the peer. Messages must arrive in the order
    /// they were sealed; a replayed, reordered or altered one fails.
    ///
    /// ```
    /// # use kevy_noise::{Error, Initiator, Keypair, Responder};
    /// # let (server, client) = (Keypair::from_secret([1; 32]), Keypair::from_secret([2; 32]));
    /// # let (m1, init) = Initiator::start(&client, &server.public(), Keypair::from_secret([3; 32]), b"", b"").unwrap();
    /// # let (_, r) = Responder::accept(&server, Keypair::from_secret([4; 32]), b"", &m1).unwrap();
    /// # let (m2, mut s) = r.finish(b"").unwrap();
    /// # let (_, mut c) = init.finish(&m2).unwrap();
    /// let sealed = c.seal(b"once").unwrap();
    /// assert_eq!(s.open(&sealed).unwrap(), b"once");
    /// assert_eq!(s.open(&sealed), Err(Error::Decrypt));
    /// ```
    pub fn open(&mut self, ciphertext: &[u8]) -> Result<Vec<u8>, Error> {
        let mut buf = ciphertext.to_vec();
        self.recv.decrypt(&[], &mut buf)?;
        Ok(buf)
    }

    /// The handshake hash both sides share; a channel binding for anything
    /// that wants to tie itself to this session.
    ///
    /// ```
    /// # use kevy_noise::{Initiator, Keypair, Responder};
    /// # let (server, client) = (Keypair::from_secret([1; 32]), Keypair::from_secret([2; 32]));
    /// # let (m1, init) = Initiator::start(&client, &server.public(), Keypair::from_secret([3; 32]), b"", b"").unwrap();
    /// # let (_, r) = Responder::accept(&server, Keypair::from_secret([4; 32]), b"", &m1).unwrap();
    /// # let (m2, s) = r.finish(b"").unwrap();
    /// # let (_, c) = init.finish(&m2).unwrap();
    /// assert_eq!(c.handshake_hash(), s.handshake_hash());
    /// ```
    pub fn handshake_hash(&self) -> [u8; 32] {
        self.hash
    }
}

/// Prefix `msg` with its length as two big-endian bytes.
///
/// ```
/// assert_eq!(kevy_noise::frame(b"ab").unwrap(), [0, 2, b'a', b'b']);
/// ```
pub fn frame(msg: &[u8]) -> Result<Vec<u8>, Error> {
    let len = u16::try_from(msg.len()).map_err(|_| Error::TooLong)?;
    let mut out = Vec::with_capacity(2 + msg.len());
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(msg);
    Ok(out)
}

/// Reassembles length-prefixed messages from a byte stream that arrives
/// in arbitrary pieces.
///
/// ```
/// let mut f = kevy_noise::Frames::default();
/// f.push(&[0, 3, b'a']);
/// assert_eq!(f.next(), None);
/// f.push(b"bc");
/// assert_eq!(f.next().as_deref(), Some(&b"abc"[..]));
/// ```
#[derive(Default)]
pub struct Frames {
    buf: Vec<u8>,
}

impl Frames {
    /// Append bytes read from the stream.
    ///
    /// ```
    /// let mut f = kevy_noise::Frames::default();
    /// f.push(&[0, 0]);
    /// assert_eq!(f.next().as_deref(), Some(&[][..]));
    /// ```
    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// The next complete message, if one has fully arrived.
    ///
    /// ```
    /// let mut f = kevy_noise::Frames::default();
    /// f.push(&[0, 1, 7, 0, 1]);
    /// assert_eq!(f.next().as_deref(), Some(&[7][..]));
    /// assert_eq!(f.next(), None);
    /// ```
    #[expect(
        clippy::should_implement_trait,
        reason = "a stream that is not exhausted by returning None"
    )]
    pub fn next(&mut self) -> Option<Vec<u8>> {
        let len = usize::from(u16::from_be_bytes([*self.buf.first()?, *self.buf.get(1)?]));
        if self.buf.len() < 2 + len {
            return None;
        }
        let msg = self.buf[2..2 + len].to_vec();
        self.buf.drain(..2 + len);
        Some(msg)
    }
}
