// Crc16 — CRC16-CCITT (XMODEM) + the Redis-cluster key→slot mapping
// (client-contract §3.15 / §7). Reproduces Redis's key_hash_slot exactly so
// client routing agrees with the server and -MOVED never fires for a
// correctly-routed key. Check vector: crc16("123456789") == 0x31C3.
package jp.golia.kevy;

public final class Crc16 {
    private Crc16() {}

    private static final int POLY = 0x1021;
    private static final int[] TABLE = makeTable();

    private static int[] makeTable() {
        int[] t = new int[256];
        for (int i = 0; i < 256; i++) {
            int crc = i << 8;
            for (int bit = 0; bit < 8; bit++) {
                crc = ((crc & 0x8000) != 0) ? ((crc << 1) ^ POLY) : (crc << 1);
            }
            t[i] = crc & 0xFFFF;
        }
        return t;
    }

    static int crc16(byte[] b) {
        int crc = 0;
        for (byte c : b) {
            crc = ((crc << 8) ^ TABLE[((crc >>> 8) ^ (c & 0xFF)) & 0xFF]) & 0xFFFF;
        }
        return crc;
    }

    /** The Redis-cluster hash slot of a key: crc16(hashtag(key)) & 16383. */
    public static int keyHashSlot(byte[] key) {
        return crc16(hashtag(key)) & 0x3FFF;
    }

    /** The {hashtag} rule: hash only the first non-empty {…} span, else the whole key. */
    static byte[] hashtag(byte[] key) {
        int open = -1;
        for (int i = 0; i < key.length; i++) {
            if (key[i] == '{') { open = i; break; }
        }
        if (open < 0) return key;
        for (int i = open + 1; i < key.length; i++) {
            if (key[i] == '}') {
                if (i > open + 1) {
                    byte[] tag = new byte[i - open - 1];
                    System.arraycopy(key, open + 1, tag, 0, tag.length);
                    return tag;
                }
                return key;
            }
        }
        return key;
    }
}
