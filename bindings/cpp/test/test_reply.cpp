// RESP2/RESP3 parser + CRC16 unit tests (contract §4.1, §3.15).
#include <string>

#include "harness.hpp"
#include "kevy/crc16.hpp"
#include "kevy/resp.hpp"

using namespace kevy;
using namespace kevy::test;

static Reply decode(const std::string& s) {
  return resp::decode_reply(reinterpret_cast<const uint8_t*>(s.data()), s.size());
}

KEVY_TEST(reply_simple_and_error) {
  Reply s = decode("+OK\r\n");
  CHECK(s.kind == ReplyKind::Simple);
  CHECK_EQ(s.str(), std::string("OK"));
  Reply e = decode("-WRONGTYPE nope\r\n");
  CHECK(e.is_error());
  CHECK_EQ(e.str(), std::string("WRONGTYPE nope"));
}

KEVY_TEST(reply_int_bulk_nil) {
  CHECK_EQ(decode(":42\r\n").integer, int64_t(42));
  CHECK_EQ(decode("$3\r\nabc\r\n").str(), std::string("abc"));
  CHECK(decode("$-1\r\n").is_nil());
  CHECK(decode("*-1\r\n").is_nil());
}

KEVY_TEST(reply_binary_safe_bulk) {
  // A bulk with an embedded NUL must round-trip intact.
  std::string wire = "$3\r\n";
  wire.push_back('a');
  wire.push_back('\0');
  wire.push_back('b');
  wire += "\r\n";
  Reply r = decode(wire);
  CHECK_EQ(r.bytes.size(), size_t(3));
  CHECK(r.bytes[1] == '\0');
}

KEVY_TEST(reply_array_nested) {
  Reply r = decode("*2\r\n:1\r\n$2\r\nhi\r\n");
  CHECK(r.kind == ReplyKind::Array);
  CHECK_EQ(r.array.size(), size_t(2));
  CHECK_EQ(r.array[0].integer, int64_t(1));
  CHECK_EQ(r.array[1].str(), std::string("hi"));
}

KEVY_TEST(reply_resp3_types) {
  CHECK(decode(",3.14\r\n").kind == ReplyKind::Double);
  CHECK(decode("#t\r\n").boolean == true);
  CHECK(decode("_\r\n").kind == ReplyKind::Null);
  Reply m = decode("%1\r\n+k\r\n:9\r\n");
  CHECK(m.kind == ReplyKind::Map);
  CHECK_EQ(m.map.size(), size_t(1));
  Reply p = decode(">3\r\n$7\r\nmessage\r\n$2\r\nch\r\n$2\r\nhi\r\n");
  CHECK(p.kind == ReplyKind::Push);
  CHECK_EQ(p.array.size(), size_t(3));
}

KEVY_TEST(reply_attribute_skipped) {
  // A |N attribute frame decorates and is transparently discarded.
  Reply r = decode("|1\r\n+ttl\r\n:10\r\n:7\r\n");
  CHECK(r.kind == ReplyKind::Int);
  CHECK_EQ(r.integer, int64_t(7));
}

KEVY_TEST(reply_reencode_roundtrip) {
  std::string wire = "*2\r\n$3\r\nfoo\r\n:5\r\n";
  Reply r = decode(wire);
  std::string out;
  resp::encode_reply(out, r);
  CHECK_EQ(out, wire);
}

KEVY_TEST(crc16_check_vector) {
  // Redis check vector: crc16("123456789") == 0x31C3.
  CHECK_EQ(crc16("123456789"), uint16_t(0x31C3));
}

KEVY_TEST(crc16_hashtag) {
  // {hashtag} makes different keys route to the same slot.
  CHECK_EQ(key_hash_slot("{user1000}.following"), key_hash_slot("{user1000}.followers"));
  // Empty hashtag falls back to the whole key.
  CHECK(key_hash_slot("{}foo") == key_hash_slot("{}foo"));
}
