#include "argv.hpp"
#include "kevy/client.hpp"
#include "kevy/errors.hpp"
#include "reply_util.hpp"

// Sorted-set algebra: ZINTERSTORE / ZUNIONSTORE / ZINTERCARD (contract §3.6).
// Available on both backends. AGGREGATE is emitted only for non-default modes;
// WEIGHTS (when given) must be one-per-key.

namespace kevy {

using detail::i2s;

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

std::vector<std::string> zstore_argv(const char* verb, std::string_view dest, const ByteList& keys,
                                     const std::optional<std::vector<double>>& weights, ZAggregate agg) {
  std::vector<std::string> argv;
  argv.reserve(keys.size() * 2 + 6);
  argv.emplace_back(verb);
  argv.emplace_back(dest);
  argv.push_back(i2s(static_cast<int64_t>(keys.size())));
  detail::push_all(argv, keys);
  if (weights.has_value()) {
    argv.emplace_back("WEIGHTS");
    for (double w : *weights) argv.push_back(detail::format_double(w));
  }
  if (agg != ZAggregate::Sum) {
    argv.emplace_back("AGGREGATE");
    argv.emplace_back(agg_tag(agg));
  }
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
  std::vector<std::string> argv;
  argv.reserve(keys.size() + 4);
  argv.emplace_back("ZINTERCARD");
  argv.push_back(i2s(static_cast<int64_t>(keys.size())));
  detail::push_all(argv, keys);
  if (limit.has_value()) {
    argv.emplace_back("LIMIT");
    argv.push_back(detail::u2s(*limit));
  }
  return exec_count(argv);
}

}  // namespace kevy
