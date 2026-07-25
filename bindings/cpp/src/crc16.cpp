#include "kevy/crc16.hpp"

#include <array>

namespace kevy {
namespace {

constexpr uint16_t kPoly = 0x1021;

std::array<uint16_t, 256> make_table() {
  std::array<uint16_t, 256> t{};
  for (int i = 0; i < 256; i++) {
    uint16_t crc = static_cast<uint16_t>(i) << 8;
    for (int bit = 0; bit < 8; bit++) {
      crc = (crc & 0x8000) ? static_cast<uint16_t>((crc << 1) ^ kPoly) : static_cast<uint16_t>(crc << 1);
    }
    t[i] = crc;
  }
  return t;
}

const std::array<uint16_t, 256> kTable = make_table();

// Redis {hashtag} rule: hash only the bytes between the first '{' and the
// first '}' after it, when that span is non-empty; otherwise the whole key.
std::string_view hashtag(std::string_view key) {
  size_t open = key.find('{');
  if (open == std::string_view::npos) return key;
  size_t close = key.find('}', open + 1);
  if (close == std::string_view::npos || close == open + 1) return key;
  return key.substr(open + 1, close - open - 1);
}

}  // namespace

uint16_t crc16(std::string_view data) {
  uint16_t crc = 0;
  for (unsigned char c : data) {
    crc = static_cast<uint16_t>((crc << 8) ^ kTable[static_cast<uint8_t>(crc >> 8) ^ c]);
  }
  return crc;
}

uint16_t key_hash_slot(std::string_view key) {
  return crc16(hashtag(key)) & 0x3FFF;
}

}  // namespace kevy
