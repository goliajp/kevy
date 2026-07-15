// CRC16 + Redis-cluster key→slot mapping (client-contract §3.15 / §7).
package jp.golia.kevy;

import static org.junit.jupiter.api.Assertions.*;

import org.junit.jupiter.api.Test;

class Crc16Test {
    @Test void checkVector() {
        assertEquals(0x31C3, Crc16.crc16(Bytes.of("123456789")));
    }

    @Test void hashtagExtraction() {
        // {tag} routes by the tag only, so these share a slot.
        assertEquals(Crc16.keyHashSlot(Bytes.of("{user1000}.following")),
            Crc16.keyHashSlot(Bytes.of("{user1000}.followers")));
        // an empty {} span falls back to hashing the whole key.
        assertNotEquals(Crc16.keyHashSlot(Bytes.of("foo{}bar")), Crc16.keyHashSlot(Bytes.of("{}")));
    }

    @Test void slotInRange() {
        int slot = Crc16.keyHashSlot(Bytes.of("anykey"));
        assertTrue(slot >= 0 && slot < 16384);
    }
}
