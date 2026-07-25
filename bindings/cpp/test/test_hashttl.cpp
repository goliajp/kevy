// Hash-field TTL (contract §6 "Hash-field TTL"), both backends.
#include <chrono>

#include "backends.hpp"
#include "harness.hpp"

using namespace kevy;
using namespace kevy::test;
using namespace std::chrono_literals;

KEVY_TEST(hashttl_hexpire_codes) {
  both_backends(t, [&](Client& c, bool) {
    c.hset("h", {{"f1", "v1"}, {"f2", "v2"}});
    auto codes = c.hexpire("h", keys({"f1", "nope"}), 100s, HExpireCond::Always);
    CHECK_EQ(codes.size(), size_t(2));
    CHECK_EQ(int(codes[0]), 1);   // deadline set
    CHECK_EQ(int(codes[1]), -2);  // missing field
  });
}

KEVY_TEST(hashttl_httl_hpttl) {
  both_backends(t, [&](Client& c, bool) {
    c.hset("h", {{"f1", "v1"}});
    c.hpexpire("h", keys({"f1"}), 100000ms, HExpireCond::Always);
    auto secs = c.httl("h", keys({"f1"}));
    CHECK_EQ(secs.size(), size_t(1));
    CHECK(secs[0] > 0 && secs[0] <= 100);
    auto ms = c.hpttl("h", keys({"f1"}));
    CHECK(ms[0] > 1000);  // ms precision >> secs
  });
}

KEVY_TEST(hashttl_hpersist) {
  both_backends(t, [&](Client& c, bool) {
    c.hset("h", {{"f1", "v1"}});
    c.hpexpire("h", keys({"f1"}), 100000ms, HExpireCond::Always);
    auto codes = c.hpersist("h", keys({"f1"}));
    CHECK_EQ(int(codes[0]), 1);  // cleared
    auto ttl = c.httl("h", keys({"f1"}));
    CHECK_EQ(ttl[0], int64_t(-1));  // no TTL now
  });
}

KEVY_TEST(hashttl_empty_fields_invalid) {
  both_backends(t, [&](Client& c, bool) {
    CHECK_THROWS_KIND(c.httl("h", keys({})), ErrorKind::InvalidInput);
    CHECK_THROWS_KIND(c.hexpire("h", keys({}), 10s, HExpireCond::Always), ErrorKind::InvalidInput);
  });
}
