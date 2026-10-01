//! Poly1305 (RFC 8439 §2.5) with three 44/44/42-bit limbs and 128-bit
//! products. No branch or index depends on the key or the message.

const M44: u64 = 0xfff_ffff_ffff;
const M42: u64 = 0x3ff_ffff_ffff;

fn le64(b: &[u8]) -> u64 {
    let mut w = [0u8; 8];
    w.copy_from_slice(&b[..8]);
    u64::from_le_bytes(w)
}

/// Incremental Poly1305 over a one-time 32-byte key.
pub(crate) struct Poly1305 {
    r: [u64; 3],
    s: [u64; 2],
    pad: [u64; 2],
    h: [u64; 3],
    buf: [u8; 16],
    buf_len: usize,
}

impl Poly1305 {
    pub(crate) fn new(key: &[u8; 32]) -> Self {
        let t0 = le64(&key[0..]);
        let t1 = le64(&key[8..]);
        let r = [
            t0 & 0xffc_0fff_ffff,
            ((t0 >> 44) | (t1 << 20)) & 0xfff_ffc0_ffff,
            (t1 >> 24) & 0x00f_ffff_fc0f,
        ];
        Poly1305 {
            r,
            s: [r[1] * (5 << 2), r[2] * (5 << 2)],
            pad: [le64(&key[16..]), le64(&key[24..])],
            h: [0; 3],
            buf: [0; 16],
            buf_len: 0,
        }
    }

    fn block(&mut self, m: &[u8], hibit: u64) {
        let [r0, r1, r2] = self.r.map(u128::from);
        let [s1, s2] = self.s.map(u128::from);
        let t0 = le64(&m[0..]);
        let t1 = le64(&m[8..]);
        let h0 = u128::from(self.h[0] + (t0 & M44));
        let h1 = u128::from(self.h[1] + (((t0 >> 44) | (t1 << 20)) & M44));
        let h2 = u128::from(self.h[2] + (((t1 >> 24) & M42) | hibit));
        let d0 = h0 * r0 + h1 * s2 + h2 * s1;
        let mut d1 = h0 * r1 + h1 * r0 + h2 * s2;
        let mut d2 = h0 * r2 + h1 * r1 + h2 * r0;
        let mut c = (d0 >> 44) as u64;
        let mut n0 = (d0 as u64) & M44;
        d1 += u128::from(c);
        c = (d1 >> 44) as u64;
        let n1 = (d1 as u64) & M44;
        d2 += u128::from(c);
        c = (d2 >> 42) as u64;
        let n2 = (d2 as u64) & M42;
        n0 += c * 5;
        c = n0 >> 44;
        self.h = [n0 & M44, n1 + c, n2];
    }

    pub(crate) fn update(&mut self, mut data: &[u8]) {
        if self.buf_len > 0 {
            let take = (16 - self.buf_len).min(data.len());
            self.buf[self.buf_len..self.buf_len + take].copy_from_slice(&data[..take]);
            self.buf_len += take;
            data = &data[take..];
            if self.buf_len < 16 {
                return;
            }
            let full = self.buf;
            self.block(&full, 1 << 40);
            self.buf_len = 0;
        }
        let (blocks, rest) = data.as_chunks::<16>();
        for b in blocks {
            self.block(b, 1 << 40);
        }
        self.buf[..rest.len()].copy_from_slice(rest);
        self.buf_len = rest.len();
    }

    pub(crate) fn finalize(mut self) -> [u8; 16] {
        if self.buf_len > 0 {
            let mut last = [0u8; 16];
            last[..self.buf_len].copy_from_slice(&self.buf[..self.buf_len]);
            last[self.buf_len] = 1;
            self.block(&last, 0);
        }
        let h = full_reduce(self.h);
        let [t0, t1] = self.pad;
        let mut h0 = h[0] + (t0 & M44);
        let mut c = h0 >> 44;
        h0 &= M44;
        let mut h1 = h[1] + (((t0 >> 44) | (t1 << 20)) & M44) + c;
        c = h1 >> 44;
        h1 &= M44;
        let h2 = (h[2] + ((t1 >> 24) & M42) + c) & M42;
        let lo = h0 | (h1 << 44);
        let hi = (h1 >> 20) | (h2 << 24);
        let mut out = [0u8; 16];
        out[..8].copy_from_slice(&lo.to_le_bytes());
        out[8..].copy_from_slice(&hi.to_le_bytes());
        out
    }
}

/// Carry `h` fully and reduce it below 2^130 - 5, without a branch.
fn full_reduce([mut h0, mut h1, mut h2]: [u64; 3]) -> [u64; 3] {
    let mut c = h1 >> 44;
    h1 &= M44;
    h2 += c;
    c = h2 >> 42;
    h2 &= M42;
    h0 += c * 5;
    c = h0 >> 44;
    h0 &= M44;
    h1 += c;
    c = h1 >> 44;
    h1 &= M44;
    h2 += c;
    c = h2 >> 42;
    h2 &= M42;
    h0 += c * 5;
    c = h0 >> 44;
    h0 &= M44;
    h1 += c;

    let mut g0 = h0 + 5;
    c = g0 >> 44;
    g0 &= M44;
    let mut g1 = h1 + c;
    c = g1 >> 44;
    g1 &= M44;
    let g2 = (h2 + c).wrapping_sub(1 << 42);
    // all ones when h + 5 did not overflow 2^130, i.e. h >= p: take g
    let take_g = (g2 >> 63).wrapping_sub(1);
    let keep_h = !take_g;
    [(h0 & keep_h) | (g0 & take_g), (h1 & keep_h) | (g1 & take_g), (h2 & keep_h) | (g2 & take_g)]
}

/// One-shot MAC of `data` under `key`.
#[cfg(test)]
pub(crate) fn mac(key: &[u8; 32], data: &[u8]) -> [u8; 16] {
    let mut p = Poly1305::new(key);
    p.update(data);
    p.finalize()
}
