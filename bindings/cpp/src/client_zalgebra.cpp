#include "argv.hpp"
#include "kevy/client.hpp"
#include "kevy/errors.hpp"
#include "reply_util.hpp"

// Sorted-set algebra: ZINTERSTORE / ZUNIONSTORE / ZINTERCARD (contract §3.6).
// Available on both backends. AGGREGATE is emitted only for non-default modes;
// WEIGHTS (when given) must be one-per-key.

namespace kevy {

using detail::Args;

namespace {

const char* agg_tag(ZAggregate a) {
  switch (a) {
    case ZAggregate::Min: return "MIN";
    case ZAggregate::Max: return "MAX";
    default: return "SUM";
  }
}

void check_source_keys(const ByteList& keys) {
  if (keys.empty()) throw InvalidInputError("zset algebra needs at least one source key");
}

Args zstore_argv(std::string_view verb, std::string_view dest, const ByteList& keys,
                 const std::optional<std::vector<double>>& weights, ZAggregate agg) {
  Args argv;
  argv.reserve(keys.size() * 2 + 6);
  argv.add(verb).add(dest).add_int(static_cast<int64_t>(keys.size())).add_all(keys);
  if (weights.has_value()) {
    argv.add("WEIGHTS");
    for (double w : *weights) argv.add_double(w);
  }
  if (agg != ZAggregate::Sum) argv.add("AGGREGATE").add(agg_tag(agg));
  return argv;
}

}  // namespace

int64_t Client::zinterstore(std::string_view dest, const ByteList& keys) {
  return zinterstore_with(dest, keys, std::nullopt, ZAggregate::Sum);
}
int64_t Client::zinterstore_with(std::string_view dest, const ByteList& keys,
                                 const std::optional<std::vector<double>>& weights, ZAggregate agg) {
  check_source_keys(keys);
  return exec_count(zstore_argv("ZINTERSTORE", dest, keys, weights, agg));
}
int64_t Client::zunionstore(std::string_view dest, const ByteList& keys) {
  return zunionstore_with(dest, keys, std::nullopt, ZAggregate::Sum);
}
int64_t Client::zunionstore_with(std::string_view dest, const ByteList& keys,
                                 const std::optional<std::vector<double>>& weights, ZAggregate agg) {
  check_source_keys(keys);
  return exec_count(zstore_argv("ZUNIONSTORE", dest, keys, weights, agg));
}

int64_t Client::zintercard(const ByteList& keys, std::optional<uint64_t> limit) {
  check_source_keys(keys);
  Args argv;
  argv.reserve(keys.size() + 4);
  argv.add("ZINTERCARD").add_int(static_cast<int64_t>(keys.size())).add_all(keys);
  if (limit.has_value()) argv.add("LIMIT").add_uint(*limit);
  return exec_count(argv);
}

}  // namespace kevy
