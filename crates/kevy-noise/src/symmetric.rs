//! CipherState and SymmetricState, as §5.1 and §5.2 of the Noise
//! specification (revision 34) define them, over ChaChaPoly and BLAKE2s.

use kevy_crypto::{aead, blake2s, hkdf};

use crate::Error;

const TAG: usize = 16;

pub(crate) struct CipherState {
    k: Option<[u8; 32]>,
    n: u64,
}

fn nonce(n: u64) -> [u8; 12] {
    let mut b = [0u8; 12];
    b[4..].copy_from_slice(&n.to_le_bytes());
    b
}

impl CipherState {
    pub(crate) fn empty() -> Self {
        CipherState { k: None, n: 0 }
    }

    pub(crate) fn keyed(k: [u8; 32]) -> Self {
        CipherState { k: Some(k), n: 0 }
    }

    /// Encrypt `buf` in place and append the tag; without a key, leave it.
    pub(crate) fn encrypt(&mut self, ad: &[u8], buf: &mut Vec<u8>) -> Result<(), Error> {
        let Some(k) = self.k else { return Ok(()) };
        // 2^64 - 1 is reserved by the specification
        if self.n == u64::MAX {
            return Err(Error::NonceExhausted);
        }
        let tag = aead::seal(&k, &nonce(self.n), ad, buf);
        buf.extend_from_slice(&tag);
        self.n += 1;
        Ok(())
    }

    /// Verify and strip the tag, decrypting `buf` in place. The nonce
    /// advances only on success.
    pub(crate) fn decrypt(&mut self, ad: &[u8], buf: &mut Vec<u8>) -> Result<(), Error> {
        let Some(k) = self.k else { return Ok(()) };
        if self.n == u64::MAX {
            return Err(Error::NonceExhausted);
        }
        let body = buf.len().checked_sub(TAG).ok_or(Error::Truncated)?;
        let mut tag = [0u8; TAG];
        tag.copy_from_slice(&buf[body..]);
        buf.truncate(body);
        aead::open(&k, &nonce(self.n), ad, buf, &tag).map_err(|_| Error::Decrypt)?;
        self.n += 1;
        Ok(())
    }
}

pub(crate) struct SymmetricState {
    ck: [u8; 32],
    h: [u8; 32],
    cs: CipherState,
}

impl SymmetricState {
    pub(crate) fn new(protocol_name: &[u8]) -> Self {
        let h = if protocol_name.len() <= 32 {
            let mut h = [0u8; 32];
            h[..protocol_name.len()].copy_from_slice(protocol_name);
            h
        } else {
            blake2s::hash(protocol_name)
        };
        SymmetricState { ck: h, h, cs: CipherState::empty() }
    }

    pub(crate) fn mix_key(&mut self, ikm: &[u8]) {
        let [ck, k, _] = hkdf::hkdf(&self.ck, ikm);
        self.ck = ck;
        self.cs = CipherState::keyed(k);
    }

    pub(crate) fn mix_hash(&mut self, data: &[u8]) {
        let mut h = blake2s::Blake2s::new(32);
        h.update(&self.h);
        h.update(data);
        h.finalize(&mut self.h);
    }

    pub(crate) fn encrypt_and_hash(&mut self, plaintext: &[u8]) -> Result<Vec<u8>, Error> {
        let mut buf = plaintext.to_vec();
        self.cs.encrypt(&self.h, &mut buf)?;
        self.mix_hash(&buf);
        Ok(buf)
    }

    pub(crate) fn decrypt_and_hash(&mut self, ciphertext: &[u8]) -> Result<Vec<u8>, Error> {
        let mut buf = ciphertext.to_vec();
        self.cs.decrypt(&self.h, &mut buf)?;
        self.mix_hash(ciphertext);
        Ok(buf)
    }

    /// The two transport keys (initiator to responder, then back), and the
    /// handshake hash.
    pub(crate) fn split(&self) -> (CipherState, CipherState, [u8; 32]) {
        let [k1, k2, _] = hkdf::hkdf(&self.ck, &[]);
        (CipherState::keyed(k1), CipherState::keyed(k2), self.h)
    }
}
