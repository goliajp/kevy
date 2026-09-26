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
        seal_with(&mut self.send, plaintext)
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
        open_with(&mut self.recv, ciphertext)
    }

    /// Separate the two directions, so one thread can seal while another
    /// opens.
    ///
    /// ```
    /// # use kevy_noise::{Initiator, Keypair, Responder};
    /// # let (server, client) = (Keypair::from_secret([1; 32]), Keypair::from_secret([2; 32]));
    /// # let (m1, init) = Initiator::start(&client, &server.public(), Keypair::from_secret([3; 32]), b"", b"").unwrap();
    /// # let (_, r) = Responder::accept(&server, Keypair::from_secret([4; 32]), b"", &m1).unwrap();
    /// # let (m2, s) = r.finish(b"").unwrap();
    /// # let (_, c) = init.finish(&m2).unwrap();
    /// let (mut c_tx, mut c_rx) = c.split();
    /// let (mut s_tx, mut s_rx) = s.split();
    /// assert_eq!(s_rx.open(&c_tx.seal(b"up").unwrap()).unwrap(), b"up");
    /// assert_eq!(c_rx.open(&s_tx.seal(b"down").unwrap()).unwrap(), b"down");
    /// ```
    pub fn split(self) -> (Sealer, Opener) {
        (Sealer { send: self.send }, Opener { recv: self.recv })
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

fn seal_with(send: &mut CipherState, plaintext: &[u8]) -> Result<Vec<u8>, Error> {
    if plaintext.len() > MAX_MESSAGE - TAG {
        return Err(Error::TooLong);
    }
    let mut buf = plaintext.to_vec();
    send.encrypt(&[], &mut buf)?;
    Ok(buf)
}

fn open_with(recv: &mut CipherState, ciphertext: &[u8]) -> Result<Vec<u8>, Error> {
    let mut buf = ciphertext.to_vec();
    recv.decrypt(&[], &mut buf)?;
    Ok(buf)
}

/// The sending half of a [`Transport`], from [`Transport::split`].
///
/// ```
/// # use kevy_noise::{Initiator, Keypair, Responder};
/// # let (server, client) = (Keypair::from_secret([1; 32]), Keypair::from_secret([2; 32]));
/// # let (m1, init) = Initiator::start(&client, &server.public(), Keypair::from_secret([3; 32]), b"", b"").unwrap();
/// # let (_, r) = Responder::accept(&server, Keypair::from_secret([4; 32]), b"", &m1).unwrap();
/// # let (m2, _) = r.finish(b"").unwrap();
/// # let (_, c) = init.finish(&m2).unwrap();
/// let (mut tx, _) = c.split();
/// assert_eq!(tx.seal(b"abc").unwrap().len(), 3 + 16);
/// ```
pub struct Sealer {
    send: CipherState,
}

impl Sealer {
    /// [`Transport::seal`], for this direction alone.
    ///
    /// ```
    /// # use kevy_noise::{Initiator, Keypair, Responder};
    /// # let (server, client) = (Keypair::from_secret([1; 32]), Keypair::from_secret([2; 32]));
    /// # let (m1, init) = Initiator::start(&client, &server.public(), Keypair::from_secret([3; 32]), b"", b"").unwrap();
    /// # let (_, r) = Responder::accept(&server, Keypair::from_secret([4; 32]), b"", &m1).unwrap();
    /// # let (m2, _) = r.finish(b"").unwrap();
    /// # let (_, c) = init.finish(&m2).unwrap();
    /// let (mut tx, _) = c.split();
    /// assert!(tx.seal(&vec![0; kevy_noise::MAX_MESSAGE]).is_err());
    /// ```
    pub fn seal(&mut self, plaintext: &[u8]) -> Result<Vec<u8>, Error> {
        seal_with(&mut self.send, plaintext)
    }
}

/// The receiving half of a [`Transport`], from [`Transport::split`].
///
/// ```
/// # use kevy_noise::{Initiator, Keypair, Responder};
/// # let (server, client) = (Keypair::from_secret([1; 32]), Keypair::from_secret([2; 32]));
/// # let (m1, init) = Initiator::start(&client, &server.public(), Keypair::from_secret([3; 32]), b"", b"").unwrap();
/// # let (_, r) = Responder::accept(&server, Keypair::from_secret([4; 32]), b"", &m1).unwrap();
/// # let (m2, s) = r.finish(b"").unwrap();
/// # let (_, mut c) = init.finish(&m2).unwrap();
/// let (_, mut rx) = s.split();
/// assert_eq!(rx.open(&c.seal(b"x").unwrap()).unwrap(), b"x");
/// ```
pub struct Opener {
    recv: CipherState,
}

impl Opener {
    /// [`Transport::open`], for this direction alone.
    ///
    /// ```
    /// # use kevy_noise::{Error, Initiator, Keypair, Responder};
    /// # let (server, client) = (Keypair::from_secret([1; 32]), Keypair::from_secret([2; 32]));
    /// # let (m1, init) = Initiator::start(&client, &server.public(), Keypair::from_secret([3; 32]), b"", b"").unwrap();
    /// # let (_, r) = Responder::accept(&server, Keypair::from_secret([4; 32]), b"", &m1).unwrap();
    /// # let (m2, s) = r.finish(b"").unwrap();
    /// # let (_, mut c) = init.finish(&m2).unwrap();
    /// let (_, mut rx) = s.split();
    /// let sealed = c.seal(b"once").unwrap();
    /// assert!(rx.open(&sealed).is_ok());
    /// assert_eq!(rx.open(&sealed), Err(Error::Decrypt));
    /// ```
    pub fn open(&mut self, ciphertext: &[u8]) -> Result<Vec<u8>, Error> {
        open_with(&mut self.recv, ciphertext)
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
