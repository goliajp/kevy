//! v2.10 — `PREFIX.DIGEST <prefix>`: order-insensitive checksum of a
//! prefix's rows for migration verification (kevy-cli digest / diff).
//! Rides the extension fan-out; per-shard XOR of per-row FNV-1a
//! digests over canonical value bytes, reduce XORs shards.

use kevy_store::Store;

use crate::cmd_index_query::{ST_BADARGS, ST_OK};

const FNV_OFFSET: u64 = 0xCBF2_9CE4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01B3;

fn fnv(h: &mut u64, bytes: &[u8]) {
    for &b in bytes {
        *h ^= u64::from(b);
        *h = h.wrapping_mul(FNV_PRIME);
    }
}

/// Canonical per-row digest: key, type tag, then type-normalized
/// value bytes (hash fields sorted, set members sorted, zset by
/// (score bits, member), list in order — order IS list identity).
fn row_digest(store: &mut Store, key: &[u8]) -> u64 {
    let mut h = FNV_OFFSET;
    fnv(&mut h, key);
    let ty = store.type_of(key);
    fnv(&mut h, ty.as_bytes());
    match ty {
        "string" => {
            if let Ok(Some(v)) = store.get(key) {
                let v = v.to_vec();
                fnv(&mut h, &v);
            }
        }
        "hash" => {
            if let Ok(flat) = store.hgetall(key) {
                let mut pairs: Vec<(&[u8], &[u8])> =
                    flat.chunks(2).map(|c| (c[0].as_slice(), c[1].as_slice())).collect();
                pairs.sort();
                for (f, v) in pairs {
                    fnv(&mut h, f);
                    fnv(&mut h, v);
                }
            }
        }
        "list" => {
            if let Ok(items) = store.lrange(key, 0, -1) {
                for i in items {
                    fnv(&mut h, &i);
                }
            }
        }
        "set" => {
            if let Ok(mut ms) = store.smembers(key) {
                ms.sort();
                for m in ms {
                    fnv(&mut h, &m);
                }
            }
        }
        "zset" => {
            if let Ok(items) = store.zrange(key, 0, -1) {
                for (member, score) in items {
                    fnv(&mut h, &score.to_bits().to_le_bytes());
                    fnv(&mut h, &member);
                }
            }
        }
        _ => {}
    }
    h
}

/// Per-shard half: `[ST_OK][count u64][xor u64]`.
pub(crate) fn extension_op(store: &mut Store, argv: &[Vec<u8>]) -> Vec<u8> {
    let Some(prefix) = argv.get(1) else {
        return vec![ST_BADARGS];
    };
    let mut pat = prefix.to_vec();
    pat.push(b'*');
    let keys = store.collect_keys(Some(&pat), None);
    let mut xor = 0u64;
    for key in &keys {
        xor ^= row_digest(store, key);
    }
    let mut chunk = vec![ST_OK];
    chunk.extend_from_slice(&(keys.len() as u64).to_le_bytes());
    chunk.extend_from_slice(&xor.to_le_bytes());
    chunk
}

/// Origin reduce: XOR shards, sum counts → `[count, hex64]`.
pub(crate) fn extension_reduce(chunks: Vec<Vec<u8>>) -> Vec<u8> {
    let mut out = Vec::new();
    let (mut count, mut xor) = (0u64, 0u64);
    for c in &chunks {
        if c.first() != Some(&ST_OK) || c.len() < 17 {
            kevy_resp::encode_error(&mut out, "ERR bad PREFIX.DIGEST arguments");
            return out;
        }
        count += u64::from_le_bytes(c[1..9].try_into().expect("8 bytes"));
        xor ^= u64::from_le_bytes(c[9..17].try_into().expect("8 bytes"));
    }
    kevy_resp::encode_array_len(&mut out, 2);
    kevy_resp::encode_integer(&mut out, count as i64);
    kevy_resp::encode_bulk(&mut out, format!("{xor:016x}").as_bytes());
    out
}
