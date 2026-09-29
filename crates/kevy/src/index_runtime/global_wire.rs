//! The bytes of one global-index delta between shards. Both ends are the
//! same binary, so the format needs no version: it is private to a running
//! server.
//!
//! `name_len u16 | name | incarnation u64 | partition u16 | op u8`, then
//! for a delete `key_len u32 | key | value`, for an upsert
//! `key_len u32 | key | has_old u8 [old] | value | n u16 | n × (present u8
//! [len u32 | bytes])` — a value as `tag u8` then 8 bytes (i64, f64 bits)
//! or `len u32 | bytes` — and for a finished build the sending shard as
//! `u16`.

use kevy_index::IndexValue;

use super::global::Delta;

pub(crate) fn encode(name: &[u8], inc: u64, p: u16, delta: &Delta) -> Vec<u8> {
    let mut out = Vec::with_capacity(64);
    put_bytes16(&mut out, name);
    out.extend_from_slice(&inc.to_le_bytes());
    out.extend_from_slice(&p.to_le_bytes());
    match delta {
        Delta::Built { from } => {
            out.push(2);
            out.extend_from_slice(&(*from as u16).to_le_bytes());
        }
        Delta::Delete { key, value } => {
            out.push(0);
            put_bytes32(&mut out, key);
            value.encode(&mut out);
        }
        Delta::Upsert { key, old, value, values } => {
            out.push(1);
            put_bytes32(&mut out, key);
            match old {
                Some(o) => {
                    out.push(1);
                    o.encode(&mut out);
                }
                None => out.push(0),
            }
            value.encode(&mut out);
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

/// One decoded message: index name, incarnation, partition, delta.
pub(crate) type Message = (Vec<u8>, u64, usize, Delta);

/// The message; `None` for bytes this encoder did not write.
pub(crate) fn decode(bytes: &[u8]) -> Option<Message> {
    let mut r = Reader(bytes);
    let name = r.bytes16()?.to_vec();
    let inc = u64::from_le_bytes(r.take(8)?.try_into().ok()?);
    let p = usize::from(u16::from_le_bytes(r.take(2)?.try_into().ok()?));
    let op = r.take(1)?[0];
    if op == 2 {
        let from = usize::from(u16::from_le_bytes(r.take(2)?.try_into().ok()?));
        return r.0.is_empty().then_some((name, inc, p, Delta::Built { from }));
    }
    let key = r.bytes32()?.to_vec();
    let delta = match op {
        0 => Delta::Delete { key, value: r.value()? },
        1 => {
            let old = match r.take(1)?[0] {
                0 => None,
                _ => Some(r.value()?),
            };
            let value = r.value()?;
            let n = u16::from_le_bytes(r.take(2)?.try_into().ok()?);
            let mut values = Vec::with_capacity(usize::from(n));
            for _ in 0..n {
                values.push(match r.take(1)?[0] {
                    0 => None,
                    _ => Some(r.bytes32()?.to_vec()),
                });
            }
            Delta::Upsert { key, old, value, values }
        }
        _ => return None,
    };
    r.0.is_empty().then_some((name, inc, p, delta))
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

    fn value(&mut self) -> Option<IndexValue> {
        Some(match self.take(1)?[0] {
            0 => IndexValue::I64(i64::from_le_bytes(self.take(8)?.try_into().ok()?)),
            1 => {
                IndexValue::F64(f64::from_bits(u64::from_le_bytes(self.take(8)?.try_into().ok()?)))
            }
            2 => IndexValue::Str(self.bytes32()?.to_vec()),
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_delta_shape_round_trips() {
        let deltas = [
            Delta::Delete { key: b"user:1".to_vec(), value: IndexValue::Str(b"eu".to_vec()) },
            Delta::Upsert {
                key: b"k".to_vec(),
                old: None,
                value: IndexValue::I64(-42),
                values: vec![],
            },
            Delta::Upsert {
                key: b"k".to_vec(),
                old: Some(IndexValue::F64(-1.0)),
                value: IndexValue::F64(2.5),
                values: vec![Some(b"tokyo".to_vec()), None, Some(Vec::new())],
            },
            Delta::Upsert {
                key: Vec::new(),
                old: Some(IndexValue::Str(Vec::new())),
                value: IndexValue::Str(b"\x00\xff".to_vec()),
                values: vec![None],
            },
            Delta::Built { from: 63 },
        ];
        for d in deltas {
            let bytes = encode(b"idx", 1 << 40, 7, &d);
            assert_eq!(decode(&bytes), Some((b"idx".to_vec(), 1 << 40, 7, d)));
            assert_eq!(decode(&bytes[..bytes.len() - 1]), None, "a cut message is refused");
        }
    }

    #[test]
    fn a_message_this_encoder_did_not_write_is_refused() {
        let good = encode(
            b"idx",
            1,
            0,
            &Delta::Upsert {
                key: b"k".to_vec(),
                old: None,
                value: IndexValue::I64(1),
                values: vec![],
            },
        );
        // the op byte sits after the name (2 + 3), incarnation (8) and partition (2)
        let mut bad_op = good.clone();
        bad_op[15] = 9;
        assert_eq!(decode(&bad_op), None);
        // the value's tag follows the key (4 + 1) and the no-old byte
        let mut bad_tag = good.clone();
        bad_tag[22] = 7;
        assert_eq!(decode(&bad_tag), None);
        let mut trailing = good;
        trailing.push(0);
        assert_eq!(decode(&trailing), None, "bytes past the message");
    }
}
