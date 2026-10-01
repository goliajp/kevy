//! BLAKE2s (RFC 7693): 32-bit words, digests of 1 to 32 bytes, optional key.
//!
//! ```
//! assert_eq!(kevy_crypto::blake2s::hash(b"").len(), 32);
//! ```

const IV: [u32; 8] = [
    0x6a09_e667,
    0xbb67_ae85,
    0x3c6e_f372,
    0xa54f_f53a,
    0x510e_527f,
    0x9b05_688c,
    0x1f83_d9ab,
    0x5be0_cd19,
];

const SIGMA: [[usize; 16]; 10] = [
    [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15],
    [14, 10, 4, 8, 9, 15, 13, 6, 1, 12, 0, 2, 11, 7, 5, 3],
    [11, 8, 12, 0, 5, 2, 15, 13, 10, 14, 3, 6, 7, 1, 9, 4],
    [7, 9, 3, 1, 13, 12, 11, 14, 2, 6, 5, 10, 4, 0, 15, 8],
    [9, 0, 5, 7, 2, 4, 10, 15, 14, 1, 11, 12, 6, 8, 3, 13],
    [2, 12, 6, 10, 0, 11, 8, 3, 4, 13, 7, 5, 15, 14, 1, 9],
    [12, 5, 1, 15, 14, 13, 4, 10, 0, 7, 6, 3, 9, 2, 8, 11],
    [13, 11, 7, 14, 12, 1, 3, 9, 5, 0, 15, 4, 8, 6, 2, 10],
    [6, 15, 14, 9, 11, 3, 0, 8, 12, 2, 13, 7, 1, 4, 10, 5],
    [10, 2, 8, 4, 7, 6, 1, 5, 15, 11, 9, 14, 3, 12, 13, 0],
];

/// Incremental BLAKE2s.
///
/// ```
/// use kevy_crypto::blake2s::Blake2s;
///
/// let mut h = Blake2s::new(32);
/// h.update(b"a");
/// h.update(b"bc");
/// let mut out = [0u8; 32];
/// h.finalize(&mut out);
/// assert_eq!(out, kevy_crypto::blake2s::hash(b"abc"));
/// ```
#[derive(Clone)]
pub struct Blake2s {
    h: [u32; 8],
    t: u64,
    buf: [u8; 64],
    buf_len: usize,
    out_len: usize,
}

/// Shows the digest length only: in keyed mode the chaining state is
/// derived from the key, and a debug print is no place for it.
///
/// ```
/// let h = kevy_crypto::blake2s::Blake2s::new_keyed(16, b"secret");
/// assert_eq!(format!("{h:?}"), "Blake2s { out_len: 16, .. }");
/// ```
impl core::fmt::Debug for Blake2s {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Blake2s").field("out_len", &self.out_len).finish_non_exhaustive()
    }
}

fn g(v: &mut [u32; 16], (a, b, c, d): (usize, usize, usize, usize), x: u32, y: u32) {
    v[a] = v[a].wrapping_add(v[b]).wrapping_add(x);
    v[d] = (v[d] ^ v[a]).rotate_right(16);
    v[c] = v[c].wrapping_add(v[d]);
    v[b] = (v[b] ^ v[c]).rotate_right(12);
    v[a] = v[a].wrapping_add(v[b]).wrapping_add(y);
    v[d] = (v[d] ^ v[a]).rotate_right(8);
    v[c] = v[c].wrapping_add(v[d]);
    v[b] = (v[b] ^ v[c]).rotate_right(7);
}

impl Blake2s {
    /// An unkeyed hasher producing `out_len` bytes (1 to 32).
    ///
    /// ```
    /// let h = kevy_crypto::blake2s::Blake2s::new(16);
    /// let mut out = [0u8; 16];
    /// h.finalize(&mut out);
    /// ```
    ///
    /// # Panics
    ///
    /// If `out_len` is 0 or above 32.
    pub fn new(out_len: usize) -> Self {
        Self::new_keyed(out_len, &[])
    }

    /// A keyed hasher (the MAC mode of RFC 7693) producing `out_len` bytes.
    /// The key is at most 32 bytes.
    ///
    /// ```
    /// use kevy_crypto::blake2s::Blake2s;
    ///
    /// let mut a = [0u8; 32];
    /// let mut b = [0u8; 32];
    /// Blake2s::new_keyed(32, b"key one").finalize(&mut a);
    /// Blake2s::new_keyed(32, b"key two").finalize(&mut b);
    /// assert_ne!(a, b);
    /// ```
    ///
    /// # Panics
    ///
    /// If `out_len` is 0 or above 32, or the key is longer than 32 bytes.
    pub fn new_keyed(out_len: usize, key: &[u8]) -> Self {
        assert!(
            (1..=32).contains(&out_len) && key.len() <= 32,
            "BLAKE2s: out_len 1..=32, key <= 32"
        );
        let mut h = IV;
        h[0] ^= 0x0101_0000 ^ ((key.len() as u32) << 8) ^ out_len as u32;
        let mut s = Blake2s { h, t: 0, buf: [0; 64], buf_len: 0, out_len };
        if !key.is_empty() {
            s.buf[..key.len()].copy_from_slice(key);
            s.buf_len = 64;
        }
        s
    }

    fn compress(&mut self, block: &[u8; 64], last: bool) {
        let mut m = [0u32; 16];
        for (i, w) in m.iter_mut().enumerate() {
            *w = u32::from_le_bytes([
                block[4 * i],
                block[4 * i + 1],
                block[4 * i + 2],
                block[4 * i + 3],
            ]);
        }
        let mut v = [0u32; 16];
        v[..8].copy_from_slice(&self.h);
        v[8..].copy_from_slice(&IV);
        v[12] ^= self.t as u32;
        v[13] ^= (self.t >> 32) as u32;
        if last {
            v[14] = !v[14];
        }
        for s in &SIGMA {
            g(&mut v, (0, 4, 8, 12), m[s[0]], m[s[1]]);
            g(&mut v, (1, 5, 9, 13), m[s[2]], m[s[3]]);
            g(&mut v, (2, 6, 10, 14), m[s[4]], m[s[5]]);
            g(&mut v, (3, 7, 11, 15), m[s[6]], m[s[7]]);
            g(&mut v, (0, 5, 10, 15), m[s[8]], m[s[9]]);
            g(&mut v, (1, 6, 11, 12), m[s[10]], m[s[11]]);
            g(&mut v, (2, 7, 8, 13), m[s[12]], m[s[13]]);
            g(&mut v, (3, 4, 9, 14), m[s[14]], m[s[15]]);
        }
        for i in 0..8 {
            self.h[i] ^= v[i] ^ v[i + 8];
        }
    }

    /// Absorb more input.
    ///
    /// ```
    /// let mut h = kevy_crypto::blake2s::Blake2s::new(32);
    /// h.update(b"part one, ");
    /// h.update(b"part two");
    /// ```
    pub fn update(&mut self, mut data: &[u8]) {
        while !data.is_empty() {
            // the last block is held back: it must be compressed as last
            if self.buf_len == 64 {
                self.t += 64;
                let block = self.buf;
                self.compress(&block, false);
                self.buf_len = 0;
            }
            let take = (64 - self.buf_len).min(data.len());
            self.buf[self.buf_len..self.buf_len + take].copy_from_slice(&data[..take]);
            self.buf_len += take;
            data = &data[take..];
        }
    }

    /// Write the digest into `out`, whose length must equal the `out_len`
    /// given at construction.
    ///
    /// ```
    /// let mut out = [0u8; 32];
    /// kevy_crypto::blake2s::Blake2s::new(32).finalize(&mut out);
    /// assert_ne!(out, [0u8; 32]);
    /// ```
    ///
    /// # Panics
    ///
    /// If `out.len()` differs from `out_len`.
    pub fn finalize(mut self, out: &mut [u8]) {
        assert_eq!(out.len(), self.out_len, "BLAKE2s: output buffer length");
        self.t += self.buf_len as u64;
        self.buf[self.buf_len..].fill(0);
        let block = self.buf;
        self.compress(&block, true);
        for (i, b) in out.iter_mut().enumerate() {
            *b = self.h[i / 4].to_le_bytes()[i % 4];
        }
    }
}

/// The 32-byte unkeyed BLAKE2s digest of `data`.
///
/// ```
/// let d = kevy_crypto::blake2s::hash(b"abc");
/// assert_eq!(d[..4], [0x50, 0x8c, 0x5e, 0x8c]);
/// ```
pub fn hash(data: &[u8]) -> [u8; 32] {
    let mut h = Blake2s::new(32);
    h.update(data);
    let mut out = [0u8; 32];
    h.finalize(&mut out);
    out
}
