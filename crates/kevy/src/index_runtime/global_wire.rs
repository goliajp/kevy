//! The bytes of one global-index delta between shards. Both ends are the
//! same binary, so the format needs no version: it is private to a running
//! server.
//!
//! `name_len u16 | name | partition u16 | op u8 | key_len u32 | key`, and
//! for an upsert `value | n u16 | n × (present u8 [len u32 | bytes])`, the
//! value as `tag u8` then 8 bytes (i64, f64 bits) or `len u32 | bytes`.

use kevy_index::IndexValue;

use super::global::Delta;

pub(crate) fn encode(name: &[u8], p: u16, delta: &Delta) -> Vec<u8> {
    let mut out = Vec::with_capacity(64);
    put_bytes16(&mut out, name);
    out.extend_from_slice(&p.to_le_bytes());
    match delta {
        Delta::Delete { key } => {
            out.push(0);
            put_bytes32(&mut out, key);
        }
        Delta::Upsert { key, value, values } => {
            out.push(1);
            put_bytes32(&mut out, key);
            match value {
                IndexValue::I64(v) => {
                    out.push(0);
                    out.extend_from_slice(&v.to_le_bytes());
                }
                IndexValue::F64(v) => {
                    out.push(1);
                    out.extend_from_slice(&v.to_bits().to_le_bytes());
                }
                IndexValue::Str(v) => {
                    out.push(2);
                    put_bytes32(&mut out, v);
                }
            }
            out.extend_from_slice(&(values.len() as u16).to_le_bytes());
            for v in values {
                match v {
                    Some(b) => {
                        out.push(1);
                        put_bytes32(&mut out, b);
                    }
                    None => out.push(0),
                }
            }
        }
    }
    out
}

/// The index name, partition and delta; `None` for bytes this encoder did
/// not write.
pub(crate) fn decode(bytes: &[u8]) -> Option<(Vec<u8>, usize, Delta)> {
    let mut r = Reader(bytes);
    let name = r.bytes16()?.to_vec();
    let p = usize::from(u16::from_le_bytes(r.take(2)?.try_into().ok()?));
    let op = r.take(1)?[0];
    let key = r.bytes32()?.to_vec();
    let delta = match op {
        0 => Delta::Delete { key },
        1 => {
            let value = match r.take(1)?[0] {
                0 => IndexValue::I64(i64::from_le_bytes(r.take(8)?.try_into().ok()?)),
                1 => {
                    IndexValue::F64(f64::from_bits(u64::from_le_bytes(r.take(8)?.try_into().ok()?)))
                }
                2 => IndexValue::Str(r.bytes32()?.to_vec()),
                _ => return None,
            };
            let n = u16::from_le_bytes(r.take(2)?.try_into().ok()?);
            let mut values = Vec::with_capacity(usize::from(n));
            for _ in 0..n {
                values.push(match r.take(1)?[0] {
                    0 => None,
                    _ => Some(r.bytes32()?.to_vec()),
                });
            }
            Delta::Upsert { key, value, values }
        }
        _ => return None,
    };
    r.0.is_empty().then_some((name, p, delta))
}

fn put_bytes16(out: &mut Vec<u8>, b: &[u8]) {
    out.extend_from_slice(&(b.len() as u16).to_le_bytes());
    out.extend_from_slice(b);
}

fn put_bytes32(out: &mut Vec<u8>, b: &[u8]) {
    out.extend_from_slice(&(b.len() as u32).to_le_bytes());
    out.extend_from_slice(b);
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let (head, rest) = self.0.split_at_checked(n)?;
        self.0 = rest;
        Some(head)
    }

    fn bytes16(&mut self) -> Option<&'a [u8]> {
        let n = u16::from_le_bytes(self.take(2)?.try_into().ok()?);
        self.take(usize::from(n))
    }

    fn bytes32(&mut self) -> Option<&'a [u8]> {
        let n = u32::from_le_bytes(self.take(4)?.try_into().ok()?);
        self.take(n as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_delta_shape_round_trips() {
        let deltas = [
            Delta::Delete { key: b"user:1".to_vec() },
            Delta::Upsert { key: b"k".to_vec(), value: IndexValue::I64(-42), values: vec![] },
            Delta::Upsert {
                key: b"k".to_vec(),
                value: IndexValue::F64(2.5),
                values: vec![Some(b"tokyo".to_vec()), None, Some(Vec::new())],
            },
            Delta::Upsert {
                key: Vec::new(),
                value: IndexValue::Str(b"\x00\xff".to_vec()),
                values: vec![None],
            },
        ];
        for d in deltas {
            let bytes = encode(b"idx", 7, &d);
            assert_eq!(decode(&bytes), Some((b"idx".to_vec(), 7, d)));
            assert_eq!(decode(&bytes[..bytes.len() - 1]), None, "a cut message is refused");
        }
    }
}
