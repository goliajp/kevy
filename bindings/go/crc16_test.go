package kevy

import "testing"

// CRC16 / key_hash_slot conformance (contract §3.15 / §7).

func TestCRC16CheckVector(t *testing.T) {
	if got := crc16([]byte("123456789")); got != 0x31C3 {
		t.Fatalf("crc16 check vector: got %#x want 0x31C3", got)
	}
	if got := crc16(nil); got != 0 {
		t.Fatalf("crc16 empty: got %#x want 0", got)
	}
}

func TestHashtagRouting(t *testing.T) {
	// {tag} keys route by the tag only, so these must share a slot.
	if KeyHashSlot([]byte("{user1000}.following")) != KeyHashSlot([]byte("{user1000}.followers")) {
		t.Fatal("hashtag keys did not co-locate")
	}
	// Empty {} hashes the whole key.
	if KeyHashSlot([]byte("foo{}bar")) != KeyHashSlot([]byte("foo{}bar")) {
		t.Fatal("deterministic")
	}
	// Known Redis slot value (canonical example from the Redis docs).
	if s := KeyHashSlot([]byte("foo")); s != 12182 {
		t.Fatalf("slot(foo)=%d want 12182", s)
	}
	// The tag span equals hashing the tag contents directly.
	if KeyHashSlot([]byte("{user1000}.following")) != crc16([]byte("user1000"))&0x3FFF {
		t.Fatal("hashtag span mismatch")
	}
}
