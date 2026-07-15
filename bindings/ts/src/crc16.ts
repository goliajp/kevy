// CRC16-CCITT (XMODEM) and the Redis-cluster key→slot mapping (contract
// §3.15 / §7). Reproduces Redis's key_hash_slot exactly so client routing
// agrees with the server. Check vector: crc16("123456789") == 0x31C3.

const POLY = 0x1021;

const TABLE = (() => {
  const t = new Uint16Array(256);
  for (let i = 0; i < 256; i++) {
    let crc = (i << 8) & 0xffff;
    for (let bit = 0; bit < 8; bit++) {
      crc = (crc & 0x8000 ? ((crc << 1) ^ POLY) : crc << 1) & 0xffff;
    }
    t[i] = crc;
  }
  return t;
})();

export function crc16(b: Uint8Array): number {
  let crc = 0;
  for (const c of b) crc = ((crc << 8) ^ TABLE[((crc >> 8) ^ c) & 0xff]!) & 0xffff;
  return crc;
}

/** The Redis-cluster hash slot of key: crc16(hashtag(key)) & 16383. */
export function keyHashSlot(key: Uint8Array): number {
  return crc16(hashtag(key)) & 0x3fff;
}

// Hash only the bytes between the first '{' and the first non-empty '}'
// after it, else the whole key (Redis {hashtag} rule).
function hashtag(key: Uint8Array): Uint8Array {
  const open = key.indexOf(0x7b); // '{'
  if (open < 0) return key;
  const close = key.indexOf(0x7d, open + 1); // '}'
  if (close < 0) return key;
  if (close > open + 1) return key.subarray(open + 1, close);
  return key;
}
