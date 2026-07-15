// Core KV round-trips (contract §6 "Core KV"), both backends.
#include <chrono>

#include "backends.hpp"
#include "harness.hpp"

using namespace kevy;
using namespace kevy::test;
using namespace std::chrono_literals;

KEVY_TEST(core_set_get_del_exists) {
  both_backends(t, [&](Client& c, bool) {
    c.set("k", "v");
    auto v = c.get("k");
    CHECK(v.has_value());
    CHECK_EQ(*v, std::string("v"));
    CHECK(!c.get("missing").has_value());
    CHECK_EQ(c.exists(keys({"k", "k", "missing"})), int64_t(2));  // repeated counts each
    CHECK_EQ(c.del(keys({"k"})), int64_t(1));
    CHECK(!c.get("k").has_value());
  });
}

KEVY_TEST(core_incr) {
  both_backends(t, [&](Client& c, bool) {
    CHECK_EQ(c.incr("n"), int64_t(1));
    CHECK_EQ(c.incr("n"), int64_t(2));
    CHECK_EQ(c.incr_by("n", 5), int64_t(7));
    CHECK_EQ(c.incr_by("n", -3), int64_t(4));
  });
}

KEVY_TEST(core_expire_persist_ttl) {
  both_backends(t, [&](Client& c, bool) {
    CHECK_EQ(c.ttl_ms("k"), int64_t(-2));  // no key
    c.set("k", "v");
    CHECK_EQ(c.ttl_ms("k"), int64_t(-1));  // no TTL
    CHECK(c.expire("k", 10000ms));
    CHECK(c.ttl_ms("k") > 0);
    CHECK(c.persist("k"));
    CHECK_EQ(c.ttl_ms("k"), int64_t(-1));
  });
}

KEVY_TEST(core_set_with_ttl_atomic) {
  both_backends(t, [&](Client& c, bool) {
    c.set_with_ttl("k", "v", 10000ms);
    CHECK(c.get("k").has_value());
    CHECK(c.ttl_ms("k") > 0);
  });
}

KEVY_TEST(core_type_and_dbsize_flush) {
  both_backends(t, [&](Client& c, bool) {
    c.set("s", "v");
    c.lpush("l", keys({"a"}));
    CHECK_EQ(c.type_of("s"), std::string("string"));
    CHECK_EQ(c.type_of("l"), std::string("list"));
    CHECK_EQ(c.type_of("nope"), std::string("none"));
    CHECK(c.dbsize() >= 2);
    c.flushall();
    CHECK_EQ(c.dbsize(), int64_t(0));
  });
}

KEVY_TEST(core_mget_mset) {
  both_backends(t, [&](Client& c, bool) {
    c.mset({{"a", "1"}, {"b", "2"}});
    auto got = c.mget(keys({"a", "missing", "b"}));
    CHECK_EQ(got.size(), size_t(3));
    CHECK(got[0].has_value() && *got[0] == "1");
    CHECK(!got[1].has_value());  // null preserved, not empty string
    CHECK(got[2].has_value() && *got[2] == "2");
  });
}
