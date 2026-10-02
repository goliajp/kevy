//! The few unsigned big-integer operations exact decimal ↔ binary
//! conversion needs: little-endian 32-bit limbs, never a leading zero limb.

use alloc::vec::Vec;
use core::cmp::Ordering;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct Big(Vec<u32>);

impl Big {
    pub(crate) fn from_u64(v: u64) -> Big {
        let mut b = Big(alloc::vec![v as u32, (v >> 32) as u32]);
        b.trim();
        b
    }

    fn trim(&mut self) {
        while self.0.last() == Some(&0) {
            self.0.pop();
        }
    }

    pub(crate) fn is_zero(&self) -> bool {
        self.0.is_empty()
    }

    pub(crate) fn bit_len(&self) -> u64 {
        self.0.last().map_or(0, |top| self.0.len() as u64 * 32 - u64::from(top.leading_zeros()))
    }

    pub(crate) fn shr(&mut self, n: u64) {
        let limbs = ((n / 32) as usize).min(self.0.len());
        self.0.drain(..limbs);
        let bits = (n % 32) as u32;
        if bits != 0 {
            for i in 0..self.0.len() {
                let hi = self.0.get(i + 1).copied().unwrap_or(0);
                self.0[i] = (self.0[i] >> bits) | (hi << (32 - bits));
            }
        }
        self.trim();
    }

    pub(crate) fn bit(&self, i: u64) -> bool {
        self.0.get((i / 32) as usize).is_some_and(|l| l >> (i % 32) & 1 == 1)
    }

    /// Whether any bit below `n` is set.
    pub(crate) fn any_below(&self, n: u64) -> bool {
        let whole = ((n / 32) as usize).min(self.0.len());
        self.0[..whole].iter().any(|&l| l != 0)
            || (!n.is_multiple_of(32)
                && self.0.get(whole).is_some_and(|l| l & ((1 << (n % 32)) - 1) != 0))
    }

    /// `self × m + a`.
    pub(crate) fn mul_add_small(&mut self, m: u32, a: u32) {
        let mut carry = u64::from(a);
        for l in &mut self.0 {
            let v = u64::from(*l) * u64::from(m) + carry;
            *l = v as u32;
            carry = v >> 32;
        }
        if carry != 0 {
            self.0.push(carry as u32);
        }
        self.trim();
    }

    pub(crate) fn mul_pow10(&mut self, mut k: u64) {
        while k >= 9 {
            self.mul_add_small(1_000_000_000, 0);
            k -= 9;
        }
        self.mul_add_small(10u32.pow(k as u32), 0);
    }

    pub(crate) fn shl(&mut self, n: u64) {
        if self.is_zero() {
            return;
        }
        let (limbs, bits) = ((n / 32) as usize, (n % 32) as u32);
        if bits != 0 {
            let mut carry = 0u32;
            for l in &mut self.0 {
                let v = (*l << bits) | carry;
                carry = *l >> (32 - bits);
                *l = v;
            }
            if carry != 0 {
                self.0.push(carry);
            }
        }
        self.0.splice(0..0, core::iter::repeat_n(0, limbs));
    }

    /// The bits from `from` up, as an integer: `self >> from`, at most 128 bits.
    pub(crate) fn bits_from(&self, from: u64) -> u128 {
        let top = self.bit_len();
        let mut v = 0u128;
        let mut i = top;
        while i > from {
            i -= 1;
            v = (v << 1) | u128::from(self.bit(i));
        }
        v
    }

    fn sub_assign(&mut self, o: &Big) {
        let mut borrow = 0i64;
        for (i, l) in self.0.iter_mut().enumerate() {
            let v = i64::from(*l) - i64::from(o.0.get(i).copied().unwrap_or(0)) - borrow;
            borrow = i64::from(v < 0);
            *l = (v + (borrow << 32)) as u32;
        }
        self.trim();
    }

    pub(crate) fn add(&self, o: &Big) -> Big {
        let mut out = Vec::with_capacity(self.0.len().max(o.0.len()) + 1);
        let mut carry = 0u64;
        for i in 0..self.0.len().max(o.0.len()) {
            let v = u64::from(self.0.get(i).copied().unwrap_or(0))
                + u64::from(o.0.get(i).copied().unwrap_or(0))
                + carry;
            out.push(v as u32);
            carry = v >> 32;
        }
        out.push(carry as u32);
        let mut b = Big(out);
        b.trim();
        b
    }

    /// `|self - o|` and whether `o` was the larger.
    pub(crate) fn abs_diff(&self, o: &Big) -> (Big, bool) {
        if self.cmp(o) == Ordering::Less {
            let mut d = o.clone();
            d.sub_assign(self);
            (d, true)
        } else {
            let mut d = self.clone();
            d.sub_assign(o);
            (d, false)
        }
    }

    /// `floor(self / d)` when it fits 128 bits, and whether a remainder
    /// was left.
    pub(crate) fn div_small_quotient(mut self, d: &Big) -> (u128, bool) {
        let span = (self.bit_len() + 1).saturating_sub(d.bit_len());
        let mut q = 0u128;
        for i in (0..span).rev() {
            let mut shifted = d.clone();
            shifted.shl(i);
            q <<= 1;
            if self.cmp(&shifted) != Ordering::Less {
                self.sub_assign(&shifted);
                q |= 1;
            }
        }
        (q, !self.is_zero())
    }

    /// Divide by `d` in place, returning the remainder.
    fn divrem_small(&mut self, d: u32) -> u32 {
        let mut rem = 0u64;
        for l in self.0.iter_mut().rev() {
            let v = (rem << 32) | u64::from(*l);
            *l = (v / u64::from(d)) as u32;
            rem = v % u64::from(d);
        }
        self.trim();
        rem as u32
    }

    /// The decimal digits, most significant first; `0` for zero.
    pub(crate) fn to_decimal(&self) -> Vec<u8> {
        let mut n = self.clone();
        let mut chunks = Vec::new();
        while !n.is_zero() {
            chunks.push(n.divrem_small(1_000_000_000));
        }
        let mut out = Vec::new();
        for (i, c) in chunks.iter().rev().enumerate() {
            let s = alloc::format!("{c}");
            if i > 0 {
                out.resize(out.len() + 9 - s.len(), b'0');
            }
            out.extend_from_slice(s.as_bytes());
        }
        if out.is_empty() {
            out.push(b'0');
        }
        out
    }
}

impl PartialOrd for Big {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}

impl Ord for Big {
    fn cmp(&self, o: &Self) -> Ordering {
        self.0.len().cmp(&o.0.len()).then_with(|| self.0.iter().rev().cmp(o.0.iter().rev()))
    }
}
