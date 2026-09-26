//! Arithmetic modulo p = 2^255 - 19 on five 51-bit limbs.
//!
//! Limbs of a value handed to `mul`, `square` or `mul_small` stay below
//! 2^54, which keeps every 128-bit product sum far from overflow. `sub`
//! adds 2p before subtracting, so its right operand must be a carried
//! value (an output of `mul`, `square` or `mul_small`), whose limbs are
//! below 2^52.

const M51: u64 = (1 << 51) - 1;

#[derive(Clone, Copy)]
pub(crate) struct Fe(pub(crate) [u64; 5]);

impl Fe {
    pub(crate) const ZERO: Fe = Fe([0; 5]);
    pub(crate) const ONE: Fe = Fe([1, 0, 0, 0, 0]);

    /// Little-endian decode; bit 255 is ignored, as RFC 7748 requires.
    pub(crate) fn from_bytes(b: &[u8; 32]) -> Fe {
        let load = |i: usize| {
            let mut w = [0u8; 8];
            w.copy_from_slice(&b[i..i + 8]);
            u64::from_le_bytes(w)
        };
        Fe([
            load(0) & M51,
            (load(6) >> 3) & M51,
            (load(12) >> 6) & M51,
            (load(19) >> 1) & M51,
            (load(24) >> 12) & M51,
        ])
    }

    /// The canonical (fully reduced) little-endian encoding.
    pub(crate) fn to_bytes(self) -> [u8; 32] {
        // curve25519-donna's contraction: carry twice, add 19, carry, then
        // offset by 2^255 - 19 so one last carry leaves t - p when t >= p
        // and t otherwise, with the 2^255 bit dropped instead of branched on.
        let mut t = carry(carry(self.0));
        t[0] += 19;
        t = carry(t);
        t[0] += (1 << 51) - 19;
        for limb in &mut t[1..] {
            *limb += (1 << 51) - 1;
        }
        for i in 0..4 {
            t[i + 1] += t[i] >> 51;
            t[i] &= M51;
        }
        t[4] &= M51;
        let words = [
            t[0] | (t[1] << 51),
            (t[1] >> 13) | (t[2] << 38),
            (t[2] >> 26) | (t[3] << 25),
            (t[3] >> 39) | (t[4] << 12),
        ];
        let mut out = [0u8; 32];
        for (i, w) in words.iter().enumerate() {
            out[8 * i..8 * i + 8].copy_from_slice(&w.to_le_bytes());
        }
        out
    }

    pub(crate) fn add(self, o: Fe) -> Fe {
        let (a, b) = (self.0, o.0);
        Fe([a[0] + b[0], a[1] + b[1], a[2] + b[2], a[3] + b[3], a[4] + b[4]])
    }

    pub(crate) fn sub(self, o: Fe) -> Fe {
        // 2p, limb by limb
        const P2: [u64; 5] = [
            0xf_ffff_ffff_ffda,
            0xf_ffff_ffff_fffe,
            0xf_ffff_ffff_fffe,
            0xf_ffff_ffff_fffe,
            0xf_ffff_ffff_fffe,
        ];
        let (a, b) = (self.0, o.0);
        Fe(carry([
            a[0] + P2[0] - b[0],
            a[1] + P2[1] - b[1],
            a[2] + P2[2] - b[2],
            a[3] + P2[3] - b[3],
            a[4] + P2[4] - b[4],
        ]))
    }

    pub(crate) fn mul(self, o: Fe) -> Fe {
        let [a0, a1, a2, a3, a4] = self.0.map(u128::from);
        let [b0, b1, b2, b3, b4] = o.0.map(u128::from);
        let (b1_19, b2_19, b3_19, b4_19) = (b1 * 19, b2 * 19, b3 * 19, b4 * 19);
        let r0 = a0 * b0 + a1 * b4_19 + a2 * b3_19 + a3 * b2_19 + a4 * b1_19;
        let r1 = a0 * b1 + a1 * b0 + a2 * b4_19 + a3 * b3_19 + a4 * b2_19;
        let r2 = a0 * b2 + a1 * b1 + a2 * b0 + a3 * b4_19 + a4 * b3_19;
        let r3 = a0 * b3 + a1 * b2 + a2 * b1 + a3 * b0 + a4 * b4_19;
        let r4 = a0 * b4 + a1 * b3 + a2 * b2 + a3 * b1 + a4 * b0;
        wide_carry([r0, r1, r2, r3, r4])
    }

    pub(crate) fn square(self) -> Fe {
        self.mul(self)
    }

    pub(crate) fn mul_small(self, n: u32) -> Fe {
        wide_carry(self.0.map(|l| u128::from(l) * u128::from(n)))
    }

    /// Swap `a` and `b` when `swap` is 1, with no branch on it.
    pub(crate) fn cswap(a: &mut Fe, b: &mut Fe, swap: u64) {
        let mask = 0u64.wrapping_sub(swap);
        for i in 0..5 {
            let x = mask & (a.0[i] ^ b.0[i]);
            a.0[i] ^= x;
            b.0[i] ^= x;
        }
    }

    fn square_n(self, n: u32) -> Fe {
        let mut r = self;
        for _ in 0..n {
            r = r.square();
        }
        r
    }

    /// self^(p - 2), the inverse for any nonzero value (and 0 for 0).
    pub(crate) fn invert(self) -> Fe {
        let z2 = self.square();
        let z9 = z2.square_n(2).mul(self);
        let z11 = z9.mul(z2);
        let z_5_0 = z11.square().mul(z9);
        let z_10_0 = z_5_0.square_n(5).mul(z_5_0);
        let z_20_0 = z_10_0.square_n(10).mul(z_10_0);
        let z_40_0 = z_20_0.square_n(20).mul(z_20_0);
        let z_50_0 = z_40_0.square_n(10).mul(z_10_0);
        let z_100_0 = z_50_0.square_n(50).mul(z_50_0);
        let z_200_0 = z_100_0.square_n(100).mul(z_100_0);
        let z_250_0 = z_200_0.square_n(50).mul(z_50_0);
        z_250_0.square_n(5).mul(z11)
    }
}

fn carry(mut t: [u64; 5]) -> [u64; 5] {
    for i in 0..4 {
        t[i + 1] += t[i] >> 51;
        t[i] &= M51;
    }
    t[0] += 19 * (t[4] >> 51);
    t[4] &= M51;
    t
}

fn wide_carry(mut r: [u128; 5]) -> Fe {
    let m = u128::from(M51);
    for i in 0..4 {
        r[i + 1] += r[i] >> 51;
        r[i] &= m;
    }
    r[0] += 19 * (r[4] >> 51);
    r[4] &= m;
    r[1] += r[0] >> 51;
    r[0] &= m;
    Fe(r.map(|l| l as u64))
}
