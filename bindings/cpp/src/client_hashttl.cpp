#include "argv.hpp"
#include "kevy/client.hpp"
#include "kevy/errors.hpp"
#include "reply_util.hpp"

// Hash-field TTL: HEXPIRE / HPEXPIRE / HPERSIST / HTTL / HPTTL (Redis 7.4
// shape, contract §3.7). Per-field replies come back in request order.
// Available on both backends.

namespace kevy {

using detail::Args;
using detail::i2s;
using detail::raise_reply_error;
using detail::raise_unexpected;

namespace {

const char* cond_keyword(HExpireCond c) {
  switch (c) {
    case HExpireCond::Nx: return "NX";
    case HExpireCond::Xx: return "XX";
    case HExpireCond::Gt: return "GT";
    case HExpireCond::Lt: return "LT";
    default: return nullptr;
  }
}

void check_fields(const ByteList& fields) {
  if (fields.empty()) throw InvalidInputError("hash field-TTL verbs need at least one field");
}

// "verb key [arg] [cond] FIELDS n field…"
Args hashttl_argv(std::string_view verb, std::string_view key, std::optional<std::string> arg,
                  HExpireCond cond, const ByteList& fields) {
  Args argv;
  argv.reserve(fields.size() + 6);
  argv.add(verb).add(key);
  if (arg.has_value()) argv.add_owned(std::move(*arg));
  if (const char* kw = cond_keyword(cond)) argv.add(kw);
  argv.add("FIELDS").add_int(static_cast<int64_t>(fields.size())).add_all(fields);
  return argv;
}

std::vector<int64_t> int_array(Reply r) {
  if (r.kind == ReplyKind::Error) raise_reply_error(r);
  if (r.kind != ReplyKind::Array) raise_unexpected(r);
  std::vector<int64_t> out;
  out.reserve(r.array.size());
  for (const auto& it : r.array) {
    if (it.kind != ReplyKind::Int) raise_unexpected(it);
    out.push_back(it.integer);
  }
  return out;
}

}  // namespace

std::vector<HExpireCode> Client::hexpire(std::string_view key, const ByteList& fields, Duration ttl,
                                         HExpireCond cond) {
  check_fields(fields);
  int64_t secs = ttl.count() / 1000;  // whole-second precision
  auto ints = int_array(exec(hashttl_argv("HEXPIRE", key, i2s(secs), cond, fields)));
  return std::vector<HExpireCode>(ints.begin(), ints.end());
}

std::vector<HExpireCode> Client::hpexpire(std::string_view key, const ByteList& fields, Duration ttl,
                                          HExpireCond cond) {
  check_fields(fields);
  int64_t ms = ttl.count() < 0 ? 0 : ttl.count();
  auto ints = int_array(exec(hashttl_argv("HPEXPIRE", key, i2s(ms), cond, fields)));
  return std::vector<HExpireCode>(ints.begin(), ints.end());
}

std::vector<HExpireCode> Client::hpersist(std::string_view key, const ByteList& fields) {
  check_fields(fields);
  auto ints = int_array(exec(hashttl_argv("HPERSIST", key, std::nullopt, HExpireCond::Always, fields)));
  return std::vector<HExpireCode>(ints.begin(), ints.end());
}

std::vector<int64_t> Client::httl(std::string_view key, const ByteList& fields) {
  check_fields(fields);
  return int_array(exec(hashttl_argv("HTTL", key, std::nullopt, HExpireCond::Always, fields)));
}

std::vector<int64_t> Client::hpttl(std::string_view key, const ByteList& fields) {
  check_fields(fields);
  return int_array(exec(hashttl_argv("HPTTL", key, std::nullopt, HExpireCond::Always, fields)));
}

}  // namespace kevy
