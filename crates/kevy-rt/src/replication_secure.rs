//! The primary side of a Noise-protected replication link. The reactor
//! keeps reading and writing plaintext exactly as before; this sits between
//! it and the socket: raw bytes in are unframed and decrypted, and pending
//! plaintext is sealed and framed just before it is written.

use kevy_noise::{Frames, Keypair, MAX_MESSAGE, Responder, Transport, frame};

const PROLOGUE: &[u8] = b"kevy-replicate\x001";
const TAG: usize = 16;

/// This node's key, and which replicas may connect.
///
/// ```
/// use kevy_noise::Keypair;
/// use kevy_rt::ReplicationSecurity;
///
/// let open = ReplicationSecurity::new(Keypair::from_secret([1; 32]));
/// assert!(open.replica_keys.is_empty()); // any replica, encrypted
/// ```
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ReplicationSecurity {
    /// The primary's static key pair; replicas are configured with its
    /// public half.
    ///
    /// ```
    /// let sec = kevy_rt::ReplicationSecurity::new(kevy_noise::Keypair::from_secret([1; 32]));
    /// assert_ne!(sec.local.public(), [0; 32]);
    /// ```
    pub local: Keypair,
    /// Replica public keys allowed to connect. Empty: any replica may, and
    /// the link is encrypted without being restricted.
    ///
    /// ```
    /// let only_one = kevy_rt::ReplicationSecurity::new(kevy_noise::Keypair::from_secret([1; 32]))
    ///     .with_replica_keys(vec![kevy_noise::Keypair::from_secret([2; 32]).public()]);
    /// assert_eq!(only_one.replica_keys.len(), 1);
    /// ```
    pub replica_keys: Vec<[u8; 32]>,
}

impl ReplicationSecurity {
    /// This node's key pair `local`, admitting any replica (the link is
    /// encrypted, not restricted). The key has no default: without it
    /// the link authenticates nobody.
    ///
    /// ```
    /// let sec = kevy_rt::ReplicationSecurity::new(kevy_noise::Keypair::from_secret([1; 32]));
    /// assert!(sec.replica_keys.is_empty());
    /// ```
    #[must_use]
    pub fn new(local: Keypair) -> Self {
        Self { local, replica_keys: Vec::new() }
    }

    /// Set [`Self::replica_keys`]: admit only replicas holding one of
    /// these public keys.
    ///
    /// ```
    /// let sec = kevy_rt::ReplicationSecurity::new(kevy_noise::Keypair::from_secret([1; 32]))
    ///     .with_replica_keys(vec![[2; 32], [3; 32]]);
    /// assert_eq!(sec.replica_keys.len(), 2);
    /// ```
    #[must_use]
    pub fn with_replica_keys(mut self, replica_keys: Vec<[u8; 32]>) -> Self {
        self.replica_keys = replica_keys;
        self
    }
}

pub(crate) enum ReplNoise {
    /// Waiting for the replica's first handshake message.
    Pending(Frames),
    Up(Box<Session>),
}

// keys and cipher state stay out of logs
impl core::fmt::Debug for ReplNoise {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ReplNoise::Pending(_) => f.write_str("ReplNoise::Pending"),
            ReplNoise::Up(s) => {
                write!(f, "ReplNoise::Up {{ unwritten: {} }}", s.wire.len() - s.wire_off)
            }
        }
    }
}

pub(crate) struct Session {
    transport: Transport,
    frames: Frames,
    wire: Vec<u8>,
    wire_off: usize,
}

fn fresh_ephemeral() -> Result<Keypair, String> {
    let mut secret = [0u8; 32];
    kevy_sys::fill_random(&mut secret).map_err(|e| format!("entropy: {e}"))?;
    Ok(Keypair::from_secret(secret))
}

impl ReplNoise {
    pub(crate) fn pending() -> Self {
        ReplNoise::Pending(Frames::default())
    }

    /// Feed bytes read from the socket; the plaintext they carry is
    /// appended to `plain`. The handshake reply, once written by the caller,
    /// is queued on the wire.
    pub(crate) fn on_bytes(
        &mut self,
        sec: &ReplicationSecurity,
        raw: &[u8],
        plain: &mut Vec<u8>,
    ) -> Result<(), String> {
        if let ReplNoise::Pending(frames) = self {
            frames.push(raw);
            let Some(first) = frames.next() else { return Ok(()) };
            let (_, resp) = Responder::accept(&sec.local, fresh_ephemeral()?, PROLOGUE, &first)
                .map_err(|e| e.to_string())?;
            let key = resp.remote_static();
            if !sec.replica_keys.is_empty()
                && !sec.replica_keys.iter().any(|k| kevy_crypto::ct_eq(k, &key))
            {
                return Err("replica key not in replica_keys".into());
            }
            let (reply, transport) = resp.finish(b"").map_err(|e| e.to_string())?;
            let wire = frame(&reply).map_err(|e| e.to_string())?;
            let frames = core::mem::take(frames);
            *self = ReplNoise::Up(Box::new(Session { transport, frames, wire, wire_off: 0 }));
            return self.on_bytes(sec, &[], plain);
        }
        if let ReplNoise::Up(s) = self {
            s.frames.push(raw);
            while let Some(msg) = s.frames.next() {
                plain.extend(s.transport.open(&msg).map_err(|e| e.to_string())?);
            }
        }
        Ok(())
    }

    /// Seal `plain` onto the wire, in messages no larger than Noise allows.
    pub(crate) fn seal(&mut self, plain: &[u8]) -> Result<(), String> {
        let ReplNoise::Up(s) = self else {
            return if plain.is_empty() {
                Ok(())
            } else {
                Err("output before the handshake".into())
            };
        };
        for chunk in plain.chunks(MAX_MESSAGE - TAG) {
            let sealed = s.transport.seal(chunk).map_err(|e| e.to_string())?;
            s.wire.extend(frame(&sealed).map_err(|e| e.to_string())?);
        }
        Ok(())
    }

    /// Framed ciphertext not yet written to the socket.
    pub(crate) fn wire(&self) -> &[u8] {
        match self {
            ReplNoise::Pending(_) => &[],
            ReplNoise::Up(s) => &s.wire[s.wire_off..],
        }
    }

    pub(crate) fn wrote(&mut self, n: usize) {
        if let ReplNoise::Up(s) = self {
            s.wire_off += n;
            if s.wire_off == s.wire.len() {
                s.wire.clear();
                s.wire_off = 0;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kevy_noise::Initiator;

    #[test]
    fn the_primary_side_handshakes_seals_and_tracks_partial_writes() {
        let primary = Keypair::from_secret([1; 32]);
        let replica = Keypair::from_secret([2; 32]);
        let sec =
            ReplicationSecurity { local: primary.clone(), replica_keys: vec![replica.public()] };
        let mut noise = ReplNoise::pending();
        assert_eq!(format!("{noise:?}"), "ReplNoise::Pending");
        assert!(noise.seal(b"too early").is_err());
        assert!(noise.seal(b"").is_ok());

        let (m1, init) = Initiator::start(
            &replica,
            &primary.public(),
            Keypair::from_secret([3; 32]),
            PROLOGUE,
            b"",
        )
        .unwrap();
        let m1 = frame(&m1).unwrap();
        let mut plain = Vec::new();
        let (head, tail) = m1.split_at(10);
        noise.on_bytes(&sec, head, &mut plain).unwrap();
        assert!(matches!(noise, ReplNoise::Pending(_)));
        noise.on_bytes(&sec, tail, &mut plain).unwrap();
        let reply = noise.wire().to_vec();
        assert!(!reply.is_empty());
        let shown = format!("{noise:?}");
        assert!(shown.contains(&format!("unwritten: {}", reply.len())));
        assert!(!shown.contains(&format!("{:?}", [1u8; 32])));

        // the reply leaves in two writes; the queue empties only after both
        noise.wrote(3);
        assert_eq!(noise.wire(), &reply[3..]);
        noise.wrote(reply.len() - 3);
        assert!(noise.wire().is_empty());
        // and the buffer is released, not kept growing behind the offset
        assert!(matches!(&noise, ReplNoise::Up(s) if s.wire.is_empty() && s.wire_off == 0));

        let mut frames = Frames::default();
        frames.push(&reply);
        let (_, mut client) = init.finish(&frames.next().unwrap()).unwrap();
        let sealed = frame(&client.seal(b"REPLICATE").unwrap()).unwrap();
        noise.on_bytes(&sec, &sealed, &mut plain).unwrap();
        assert_eq!(plain, b"REPLICATE");
        noise.seal(b"+ACK").unwrap();
        frames.push(noise.wire());
        assert_eq!(client.open(&frames.next().unwrap()).unwrap(), b"+ACK");
    }
}
