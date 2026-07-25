package kevy

// CRC16-CCITT (XMODEM) and the Redis-cluster key→slot mapping (contract
// §3.15 / §7). Reproduces Redis's key_hash_slot exactly so client routing
// agrees with the server and -MOVED never fires for a correctly-routed
// key. Check vector: crc16("123456789") == 0x31C3.

const crcPoly = 0x1021

var crcTable = makeCRCTable()

func makeCRCTable() [256]uint16 {
	var t [256]uint16
	for i := 0; i < 256; i++ {
		crc := uint16(i) << 8
		for bit := 0; bit < 8; bit++ {
			if crc&0x8000 != 0 {
				crc = (crc << 1) ^ crcPoly
			} else {
				crc <<= 1
			}
		}
		t[i] = crc
	}
	return t
}

func crc16(b []byte) uint16 {
	var crc uint16
	for _, c := range b {
		crc = (crc << 8) ^ crcTable[byte(crc>>8)^c]
	}
	return crc
}

// KeyHashSlot returns the Redis-cluster hash slot of key:
// crc16(hashtag(key)) & 16383.
func KeyHashSlot(key []byte) uint16 {
	return crc16(hashtag(key)) & 0x3FFF
}

// hashtag implements the Redis {hashtag} rule: hash only the bytes between
// the first '{' and the first '}' after it, when that span is non-empty;
// otherwise hash the whole key.
func hashtag(key []byte) []byte {
	open := -1
	for i, b := range key {
		if b == '{' {
			open = i
			break
		}
	}
	if open < 0 {
		return key
	}
	rest := key[open+1:]
	for i, b := range rest {
		if b == '}' {
			if i > 0 {
				return rest[:i]
			}
			return key
		}
	}
	return key
}
