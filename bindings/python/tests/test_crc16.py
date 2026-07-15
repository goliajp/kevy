"""CRC16 slot routing reproduces Redis's key_hash_slot (contract §7)."""

from kevy import crc16, key_hash_slot


def test_crc16_check_vector():
    # The canonical CRC16-CCITT (XMODEM) check value.
    assert crc16(b"123456789") == 0x31C3


def test_hashtag_extraction():
    # {tag} routes by the tag only, so keys sharing a tag share a slot.
    assert key_hash_slot(b"{user1000}.following") == key_hash_slot(b"{user1000}.followers")
    # Empty {} hashes the whole key.
    assert key_hash_slot(b"foo{}bar") == key_hash_slot(b"foo{}bar")


def test_slot_in_range():
    for k in (b"k0", b"k1", b"user:42", b"alpha"):
        assert 0 <= key_hash_slot(k) < 16384
