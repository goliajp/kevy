// Collections: hash / list / set / zset (contract §6 "Collections"), both backends.
#include <algorithm>

#include "backends.hpp"
#include "harness.hpp"

using namespace kevy;
using namespace kevy::test;

static bool contains(const std::vector<std::string>& v, const std::string& x) {
  return std::find(v.begin(), v.end(), x) != v.end();
}

KEVY_TEST(hash_ops) {
  both_backends(t, [&](Client& c, bool) {
    CHECK_EQ(c.hset("h", {{"f1", "v1"}, {"f2", "v2"}}), int64_t(2));  // newly-added
    CHECK_EQ(c.hset("h", {{"f1", "v1b"}}), int64_t(0));               // overwrite not counted
    auto v = c.hget("h", "f1");
    CHECK(v.has_value() && *v == "v1b");
    CHECK(!c.hget("h", "nope").has_value());
    CHECK_EQ(c.hlen("h"), int64_t(2));
    auto all = c.hgetall("h");
    CHECK_EQ(all.size(), size_t(4));  // flat [f,v,f,v]
    CHECK_EQ(c.hkeys("h").size(), size_t(2));
    CHECK_EQ(c.hvals("h").size(), size_t(2));
    CHECK_EQ(c.hdel("h", keys({"f1"})), int64_t(1));
  });
}

KEVY_TEST(list_ops) {
  both_backends(t, [&](Client& c, bool) {
    CHECK_EQ(c.rpush("l", keys({"a", "b", "c"})), int64_t(3));
    CHECK_EQ(c.lpush("l", keys({"z"})), int64_t(4));  // [z,a,b,c]
    CHECK_EQ(c.llen("l"), int64_t(4));
    auto r = c.lrange("l", 0, -1);
    CHECK_EQ(r.size(), size_t(4));
    CHECK_EQ(r[0], std::string("z"));
    CHECK_EQ(r[3], std::string("c"));
    auto neg = c.lrange("l", -2, -1);
    CHECK_EQ(neg.size(), size_t(2));
    CHECK_EQ(neg[0], std::string("b"));
    auto popped = c.lpop("l", 1);
    CHECK_EQ(popped.size(), size_t(1));
    CHECK_EQ(popped[0], std::string("z"));
    auto tail = c.rpop("l", 2);
    CHECK_EQ(tail.size(), size_t(2));
    CHECK_EQ(tail[0], std::string("c"));
  });
}

KEVY_TEST(set_ops) {
  both_backends(t, [&](Client& c, bool) {
    CHECK_EQ(c.sadd("s", keys({"a", "b", "c"})), int64_t(3));
    CHECK_EQ(c.sadd("s", keys({"a"})), int64_t(0));
    CHECK_EQ(c.scard("s"), int64_t(3));
    CHECK(c.sismember("s", "a"));
    CHECK(!c.sismember("s", "z"));
    CHECK_EQ(c.smembers("s").size(), size_t(3));
    CHECK_EQ(c.srem("s", keys({"a"})), int64_t(1));
    // combines
    c.sadd("s1", keys({"a", "b", "c"}));
    c.sadd("s2", keys({"b", "c", "d"}));
    auto inter = c.sinter(keys({"s1", "s2"}));
    CHECK_EQ(inter.size(), size_t(2));
    CHECK(contains(inter, "b") && contains(inter, "c"));
    CHECK_EQ(c.sunion(keys({"s1", "s2"})).size(), size_t(4));
    auto diff = c.sdiff(keys({"s1", "s2"}));
    CHECK_EQ(diff.size(), size_t(1));
    CHECK_EQ(diff[0], std::string("a"));
  });
}

KEVY_TEST(zset_ops) {
  both_backends(t, [&](Client& c, bool) {
    CHECK_EQ(c.zadd("z", {{1.0, "a"}, {3.0, "b"}, {2.0, "c"}}), int64_t(3));
    auto score = c.zscore("z", "b");
    CHECK(score.has_value() && *score == 3.0);
    CHECK(!c.zscore("z", "nope").has_value());
    CHECK_EQ(c.zcard("z"), int64_t(3));
    auto r = c.zrange("z", 0, -1);  // ascending score: a,c,b
    CHECK_EQ(r.size(), size_t(3));
    CHECK_EQ(r[0], std::string("a"));
    CHECK_EQ(r[1], std::string("c"));
    CHECK_EQ(r[2], std::string("b"));
    CHECK_EQ(c.zrem("z", keys({"a"})), int64_t(1));
  });
}
