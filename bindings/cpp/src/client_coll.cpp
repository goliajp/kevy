#include "argv.hpp"
#include "kevy/client.hpp"
#include "kevy/errors.hpp"
#include "reply_util.hpp"

// Collection-typed commands: hash, list, set, sorted set (contract
// §3.2–§3.5). Each runs the same argv against either backend; the embedded
// engine dispatches the full verb surface through the FFI cmd path, so set
// combines (SINTER/SUNION/SDIFF) run server-side there too.

namespace kevy {

using detail::i2s;
using detail::raise_reply_error;
using detail::raise_unexpected;
using detail::u2s;
using detail::verb_key_list;
using detail::verb_list;

// --- hash (§3.2) --------------------------------------------------------

int64_t Client::hset(std::string_view key,
                     const std::vector<std::pair<std::string_view, std::string_view>>& pairs) {
  std::vector<std::string> argv;
  argv.reserve(pairs.size() * 2 + 2);
  argv.emplace_back("HSET");
  argv.emplace_back(key);
  for (auto& [f, v] : pairs) {
    argv.emplace_back(f);
    argv.emplace_back(v);
  }
  return exec_count(argv);
}

OptBytes Client::hget(std::string_view key, std::string_view field) {
  return exec_opt_bulk({"HGET", std::string(key), std::string(field)});
}
int64_t Client::hdel(std::string_view key, const ByteList& fields) {
  return exec_count(verb_key_list("HDEL", key, fields));
}
int64_t Client::hlen(std::string_view key) { return exec_count({"HLEN", std::string(key)}); }
std::vector<std::string> Client::hgetall(std::string_view key) {
  return exec_bulks({"HGETALL", std::string(key)});
}
std::vector<std::string> Client::hkeys(std::string_view key) {
  return exec_bulks({"HKEYS", std::string(key)});
}
std::vector<std::string> Client::hvals(std::string_view key) {
  return exec_bulks({"HVALS", std::string(key)});
}

// --- list (§3.3) --------------------------------------------------------

int64_t Client::lpush(std::string_view key, const ByteList& values) {
  return exec_count(verb_key_list("LPUSH", key, values));
}
int64_t Client::rpush(std::string_view key, const ByteList& values) {
  return exec_count(verb_key_list("RPUSH", key, values));
}

std::vector<std::string> Client::lpop(std::string_view key, uint64_t count) {
  Reply r = exec({"LPOP", std::string(key), u2s(count)});
  switch (r.kind) {
    case ReplyKind::Array:
    case ReplyKind::Set: return detail::array_to_bulks(r);
    case ReplyKind::Bulk: return {r.bytes};
    case ReplyKind::Nil:
    case ReplyKind::Null: return {};
    case ReplyKind::Error: raise_reply_error(r);
    default: raise_unexpected(r);
  }
}
std::vector<std::string> Client::rpop(std::string_view key, uint64_t count) {
  Reply r = exec({"RPOP", std::string(key), u2s(count)});
  switch (r.kind) {
    case ReplyKind::Array:
    case ReplyKind::Set: return detail::array_to_bulks(r);
    case ReplyKind::Bulk: return {r.bytes};
    case ReplyKind::Nil:
    case ReplyKind::Null: return {};
    case ReplyKind::Error: raise_reply_error(r);
    default: raise_unexpected(r);
  }
}
int64_t Client::llen(std::string_view key) { return exec_count({"LLEN", std::string(key)}); }
std::vector<std::string> Client::lrange(std::string_view key, int64_t start, int64_t stop) {
  return exec_bulks({"LRANGE", std::string(key), i2s(start), i2s(stop)});
}

// --- set (§3.4) ---------------------------------------------------------

int64_t Client::sadd(std::string_view key, const ByteList& members) {
  return exec_count(verb_key_list("SADD", key, members));
}
int64_t Client::srem(std::string_view key, const ByteList& members) {
  return exec_count(verb_key_list("SREM", key, members));
}
std::vector<std::string> Client::smembers(std::string_view key) {
  return exec_bulks({"SMEMBERS", std::string(key)});
}
int64_t Client::scard(std::string_view key) { return exec_count({"SCARD", std::string(key)}); }
bool Client::sismember(std::string_view key, std::string_view member) {
  return exec_bool({"SISMEMBER", std::string(key), std::string(member)});
}
std::vector<std::string> Client::sinter(const ByteList& keys) { return exec_bulks(verb_list("SINTER", keys)); }
std::vector<std::string> Client::sunion(const ByteList& keys) { return exec_bulks(verb_list("SUNION", keys)); }
std::vector<std::string> Client::sdiff(const ByteList& keys) { return exec_bulks(verb_list("SDIFF", keys)); }

// --- sorted set (§3.5) --------------------------------------------------

int64_t Client::zadd(std::string_view key, const std::vector<ZMember>& members) {
  std::vector<std::string> argv;
  argv.reserve(members.size() * 2 + 2);
  argv.emplace_back("ZADD");
  argv.emplace_back(key);
  for (const auto& m : members) {
    argv.push_back(detail::format_double(m.score));
    argv.push_back(m.member);
  }
  return exec_count(argv);
}
int64_t Client::zrem(std::string_view key, const ByteList& members) {
  return exec_count(verb_key_list("ZREM", key, members));
}
std::optional<double> Client::zscore(std::string_view key, std::string_view member) {
  Reply r = exec({"ZSCORE", std::string(key), std::string(member)});
  switch (r.kind) {
    case ReplyKind::Bulk: return detail::parse_float_bytes(r.bytes);
    case ReplyKind::Double: return r.dbl;
    case ReplyKind::Nil:
    case ReplyKind::Null: return std::nullopt;
    case ReplyKind::Error: raise_reply_error(r);
    default: raise_unexpected(r);
  }
}
int64_t Client::zcard(std::string_view key) { return exec_count({"ZCARD", std::string(key)}); }
std::vector<std::string> Client::zrange(std::string_view key, int64_t start, int64_t stop) {
  return exec_bulks({"ZRANGE", std::string(key), i2s(start), i2s(stop)});
}

}  // namespace kevy
