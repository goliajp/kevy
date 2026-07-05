//! Distance metrics + the wire vector format (RFC D1/D3).

/// Distance metric. Scores are "smaller = closer" for every variant
/// (cosine → `1 - cos`, ip → `-dot`), so one ascending merge works.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Distance {
    /// Cosine distance (vectors pre-normalized at insert).
    #[default]
    Cosine,
    /// Squared euclidean.
    L2,
    /// Negative inner product.
    Ip,
}

impl Distance {
    /// Tag for sidecar round-trip.
    pub fn tag(self) -> &'static str {
        match self {
            Distance::Cosine => "cosine",
            Distance::L2 => "l2",
            Distance::Ip => "ip",
        }
    }

    /// Parse a tag (ASCII case-insensitive).
    pub fn parse(raw: &[u8]) -> Option<Distance> {
        if raw.eq_ignore_ascii_case(b"cosine") {
            Some(Distance::Cosine)
        } else if raw.eq_ignore_ascii_case(b"l2") {
            Some(Distance::L2)
        } else if raw.eq_ignore_ascii_case(b"ip") {
            Some(Distance::Ip)
        } else {
            None
        }
    }

    /// Distance between two prepared vectors (see [`prepare`]).
    #[inline]
    pub(crate) fn eval(self, a: &[f32], b: &[f32]) -> f32 {
        match self {
            // prepared cosine vectors are unit length → 1 - dot
            Distance::Cosine => 1.0 - dot(a, b),
            Distance::L2 => l2(a, b),
            Distance::Ip => -dot(a, b),
        }
    }

    /// Normalize a vector into its stored/query form (cosine only).
    pub(crate) fn prepare(self, v: &mut [f32]) {
        if self == Distance::Cosine {
            let norm = dot(v, v).sqrt();
            if norm > 0.0 {
                for x in v.iter_mut() {
                    *x /= norm;
                }
            }
        }
    }
}

// A single-accumulator float reduction is a loop-carried dependency
// the compiler may NOT reassociate — it compiles to a SCALAR add
// chain. Eight independent lanes let it auto-vectorize at whatever
// SIMD width the target enables (perf-record put the distance kernel
// inside the 65% beam-search dominator on the KNN shape).
#[inline]
fn dot(a: &[f32], b: &[f32]) -> f32 {
    let mut acc = [0.0f32; 8];
    let (ac, ar) = a.split_at(a.len() - a.len() % 8);
    let (bc, br) = b.split_at(ac.len().min(b.len()));
    for (ca, cb) in ac.chunks_exact(8).zip(bc.chunks_exact(8)) {
        for i in 0..8 {
            acc[i] += ca[i] * cb[i];
        }
    }
    let mut s: f32 = acc.iter().sum();
    for (x, y) in ar.iter().zip(br) {
        s += x * y;
    }
    s
}

#[inline]
fn l2(a: &[f32], b: &[f32]) -> f32 {
    let mut acc = [0.0f32; 8];
    let (ac, ar) = a.split_at(a.len() - a.len() % 8);
    let (bc, br) = b.split_at(ac.len().min(b.len()));
    for (ca, cb) in ac.chunks_exact(8).zip(bc.chunks_exact(8)) {
        for i in 0..8 {
            let d = ca[i] - cb[i];
            acc[i] += d * d;
        }
    }
    let mut s: f32 = acc.iter().sum();
    for (x, y) in ar.iter().zip(br) {
        let d = x - y;
        s += d * d;
    }
    s
}

/// Decode a wire vector: raw f32 LE bytes (`len == dim*4`), or the
/// debug form `csv:1.0,2.5,…`. `None` on any mismatch.
pub fn parse_vector(raw: &[u8], dim: usize) -> Option<Vec<f32>> {
    if let Some(csv) = raw.strip_prefix(b"csv:") {
        let vals: Option<Vec<f32>> = std::str::from_utf8(csv)
            .ok()?
            .split(',')
            .map(|s| s.trim().parse::<f32>().ok())
            .collect();
        let vals = vals?;
        return (vals.len() == dim && vals.iter().all(|x| x.is_finite())).then_some(vals);
    }
    if raw.len() != dim * 4 {
        return None;
    }
    let mut out = Vec::with_capacity(dim);
    for chunk in raw.chunks_exact(4) {
        let x = f32::from_le_bytes(chunk.try_into().expect("4 bytes"));
        if !x.is_finite() {
            return None;
        }
        out.push(x);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metrics_smaller_is_closer() {
        let mut a = vec![1.0, 0.0];
        let mut b = vec![0.9, 0.1];
        let mut c = vec![-1.0, 0.0];
        for v in [&mut a, &mut b, &mut c] {
            Distance::Cosine.prepare(v);
        }
        assert!(Distance::Cosine.eval(&a, &b) < Distance::Cosine.eval(&a, &c));
        assert!(Distance::L2.eval(&[0.0, 0.0], &[1.0, 1.0]) > Distance::L2.eval(&[0.0, 0.0], &[0.5, 0.5]));
        assert!(Distance::Ip.eval(&[1.0, 1.0], &[2.0, 2.0]) < Distance::Ip.eval(&[1.0, 1.0], &[0.1, 0.1]));
    }

    #[test]
    fn wire_formats() {
        let mut raw = Vec::new();
        for x in [1.0f32, -2.5, 3.25] {
            raw.extend_from_slice(&x.to_le_bytes());
        }
        assert_eq!(parse_vector(&raw, 3), Some(vec![1.0, -2.5, 3.25]));
        assert_eq!(parse_vector(&raw, 4), None, "dim mismatch");
        assert_eq!(parse_vector(b"csv:1, 2.5, -3", 3), Some(vec![1.0, 2.5, -3.0]));
        assert_eq!(parse_vector(b"csv:1,x,3", 3), None);
        let mut nan = Vec::new();
        for x in [1.0f32, f32::NAN] {
            nan.extend_from_slice(&x.to_le_bytes());
        }
        assert_eq!(parse_vector(&nan, 2), None, "non-finite rejected");
        assert!(Distance::parse(b"COSINE") == Some(Distance::Cosine));
        assert!(Distance::parse(b"nope").is_none());
    }
}
