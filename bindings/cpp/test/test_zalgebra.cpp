// Sorted-set algebra (contract §6 "Sorted-set algebra"), both backends.
#include "backends.hpp"
#include "harness.hpp"

using namespace kevy;
using namespace kevy::test;

KEVY_TEST(zalgebra_interstore_unionstore) {
  both_backends(t, [&](Client& c, bool) {
    c.zadd("a", {{1.0, "x"}, {2.0, "y"}});
    c.zadd("b", {{3.0, "y"}, {4.0, "z"}});
    CHECK_EQ(c.zinterstore("dest_i", keys({"a", "b"})), int64_t(1));  // {y}
    CHECK_EQ(c.zunionstore("dest_u", keys({"a", "b"})), int64_t(3));  // {x,y,z}
    // y = 2 + 3 = 5 under SUM (default)
    auto sy = c.zscore("dest_u", "y");
    CHECK(sy.has_value() && *sy == 5.0);
  });
}

KEVY_TEST(zalgebra_with_weights_aggregate) {
  both_backends(t, [&](Client& c, bool) {
    c.zadd("a", {{1.0, "y"}});
    c.zadd("b", {{3.0, "y"}});
    std::vector<double> w = {2.0, 1.0};
    c.zunionstore_with("d_sum", keys({"a", "b"}), w, ZAggregate::Sum);   // 1*2 + 3*1 = 5
    auto s = c.zscore("d_sum", "y");
    CHECK(s.has_value() && *s == 5.0);
    c.zunionstore_with("d_max", keys({"a", "b"}), w, ZAggregate::Max);   // max(2, 3) = 3
    auto mx = c.zscore("d_max", "y");
    CHECK(mx.has_value() && *mx == 3.0);
    c.zunionstore_with("d_min", keys({"a", "b"}), w, ZAggregate::Min);   // min(2, 3) = 2
    auto mn = c.zscore("d_min", "y");
    CHECK(mn.has_value() && *mn == 2.0);
  });
}

KEVY_TEST(zalgebra_intercard) {
  both_backends(t, [&](Client& c, bool) {
    c.zadd("a", {{1.0, "x"}, {2.0, "y"}, {3.0, "z"}});
    c.zadd("b", {{1.0, "y"}, {2.0, "z"}, {3.0, "w"}});
    CHECK_EQ(c.zintercard(keys({"a", "b"}), std::nullopt), int64_t(2));  // {y,z}
    CHECK_EQ(c.zintercard(keys({"a", "b"}), std::optional<uint64_t>(1)), int64_t(1));  // short-circuit
  });
}

KEVY_TEST(zalgebra_empty_keys_invalid) {
  both_backends(t, [&](Client& c, bool) {
    CHECK_THROWS_KIND(c.zinterstore("d", keys({})), ErrorKind::InvalidInput);
    CHECK_THROWS_KIND(c.zintercard(keys({}), std::nullopt), ErrorKind::InvalidInput);
  });
}
