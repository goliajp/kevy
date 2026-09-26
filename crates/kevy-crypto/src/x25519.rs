//! X25519 (RFC 7748 §5): the Montgomery ladder over Curve25519.
//!
//! ```
//! let public = kevy_crypto::x25519::x25519(&[9; 32], &kevy_crypto::x25519::BASEPOINT);
//! assert_ne!(public, [0; 32]);
//! ```

use crate::fe25519::Fe;

/// The u-coordinate of the base point, 9.
///
/// ```
/// assert_eq!(kevy_crypto::x25519::BASEPOINT[0], 9);
/// ```
pub const BASEPOINT: [u8; 32] = {
    let mut b = [0u8; 32];
    b[0] = 9;
    b
};

/// `scalar` times the point with u-coordinate `u`, as RFC 7748 defines it:
/// the scalar is clamped and bit 255 of `u` is ignored.
///
/// The result is all zeros when `u` is a point of small order; a protocol
/// that must refuse those checks for it.
///
/// ```
/// use kevy_crypto::x25519::{x25519, BASEPOINT};
///
/// let a = [1u8; 32];
/// let b = [2u8; 32];
/// let (pa, pb) = (x25519(&a, &BASEPOINT), x25519(&b, &BASEPOINT));
/// assert_eq!(x25519(&a, &pb), x25519(&b, &pa));
/// ```
pub fn x25519(scalar: &[u8; 32], u: &[u8; 32]) -> [u8; 32] {
    let mut k = *scalar;
    k[0] &= 248;
    k[31] &= 127;
    k[31] |= 64;
    ladder(&k, Fe::from_bytes(u)).to_bytes()
}

fn ladder(k: &[u8; 32], x1: Fe) -> Fe {
    let (mut x2, mut z2, mut x3, mut z3) = (Fe::ONE, Fe::ZERO, x1, Fe::ONE);
    let mut swap = 0u64;
    for t in (0..255).rev() {
        let bit = u64::from((k[t / 8] >> (t % 8)) & 1);
        swap ^= bit;
        Fe::cswap(&mut x2, &mut x3, swap);
        Fe::cswap(&mut z2, &mut z3, swap);
        swap = bit;

        let a = x2.add(z2);
        let aa = a.square();
        let b = x2.sub(z2);
        let bb = b.square();
        let e = aa.sub(bb);
        let c = x3.add(z3);
        let d = x3.sub(z3);
        let da = d.mul(a);
        let cb = c.mul(b);
        x3 = da.add(cb).square();
        z3 = x1.mul(da.sub(cb).square());
        x2 = aa.mul(bb);
        z2 = e.mul(aa.add(e.mul_small(121_665)));
    }
    Fe::cswap(&mut x2, &mut x3, swap);
    Fe::cswap(&mut z2, &mut z3, swap);
    x2.mul(z2.invert())
}
