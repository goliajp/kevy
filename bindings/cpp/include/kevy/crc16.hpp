// kevy/crc16.hpp — CRC16-CCITT (XMODEM) + Redis-cluster key→slot mapping
// (contract §3.15 / §7). Reproduces Redis's key_hash_slot exactly so client
// routing agrees with the server. Check vector: crc16("123456789") == 0x31C3.
#ifndef KEVY_CRC16_HPP
#define KEVY_CRC16_HPP

#include <cstdint>
#include <string_view>

namespace kevy {

uint16_t crc16(std::string_view data);

// Redis-cluster hash slot of key: crc16(hashtag(key)) & 16383.
uint16_t key_hash_slot(std::string_view key);

}  // namespace kevy

#endif  // KEVY_CRC16_HPP
