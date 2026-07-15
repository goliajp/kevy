// Blocking pops (contract §6 "Blocking pops"), both backends.
#include <chrono>

#include "backends.hpp"
#include "harness.hpp"

using namespace kevy;
using namespace kevy::test;
using namespace std::chrono_literals;

KEVY_TEST(blocking_blpop_immediate) {
  both_backends(t, [&](Client& c, bool) {
    c.rpush("l", keys({"a", "b"}));
    auto hit = c.blpop(keys({"l"}), 1000ms);
    CHECK(hit.has_value());
    CHECK_EQ(hit->key, std::string("l"));
    CHECK_EQ(hit->value, std::string("a"));
    auto tail = c.brpop(keys({"l"}), 1000ms);
    CHECK(tail.has_value());
    CHECK_EQ(tail->value, std::string("b"));
  });
}

KEVY_TEST(blocking_timeout_miss) {
  both_backends(t, [&](Client& c, bool) {
    auto miss = c.blpop(keys({"empty"}), 100ms);
    CHECK(!miss.has_value());  // timed out → null
  });
}

KEVY_TEST(blocking_bzpopmin) {
  both_backends(t, [&](Client& c, bool) {
    c.zadd("z", {{2.0, "b"}, {1.0, "a"}, {3.0, "c"}});
    auto hit = c.bzpopmin(keys({"z"}), 1000ms);
    CHECK(hit.has_value());
    CHECK_EQ(hit->key, std::string("z"));
    CHECK_EQ(hit->member, std::string("a"));  // lowest score
    CHECK(hit->score == 1.0);
  });
}

KEVY_TEST(blocking_invalid_args) {
  both_backends(t, [&](Client& c, bool) {
    CHECK_THROWS_KIND(c.blpop(keys({}), 100ms), ErrorKind::InvalidInput);              // empty keys
    CHECK_THROWS_KIND(c.blpop(keys({"l"}), std::optional<Duration>(0ms)), ErrorKind::InvalidInput);  // Some(0)
  });
}
