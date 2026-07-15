#include "kevy/client.hpp"

#include "argv.hpp"
#include "kevy/errors.hpp"
#include "reply_util.hpp"
#include "resp_conn.hpp"
#include "url.hpp"

namespace kevy {

using detail::i2s;
using detail::raise_reply_error;
using detail::raise_unexpected;
using detail::u2s;
using detail::verb_key_list;
using detail::verb_list;

// --- lifecycle ----------------------------------------------------------

Client Client::connect(const std::string& url) {
  detail::Target t = detail::parse_connect_url(url);
  Client c;
  c.url_ = url;
  if (t.kind == detail::TargetKind::Remote) {
    c.remote_ = detail::RespConn::dial(t.host, t.port);
    if (t.db.has_value()) c.remote_->select_db(*t.db);
    return c;
  }
  c.emb_ = detail::resolve_store(t);
  return c;
}

Client::~Client() { close(); }
Client::Client(Client&&) noexcept = default;
Client& Client::operator=(Client&&) noexcept = default;

void Client::close() {
  if (remote_) {
    remote_->close();
    remote_.reset();
  }
  emb_.reset();
}

// --- dispatch + adapters ------------------------------------------------

Reply Client::exec(const std::vector<std::string>& argv) {
  if (emb_) return emb_->cmd(argv);
  if (remote_) return remote_->request(argv);
  throw ClosedError();
}

detail::RespConn* Client::require_remote(const char* feature) {
  if (remote_) return remote_.get();
  throw UnsupportedError(std::string(feature) +
                         " is remote-only; on the embedded backend use command() (raw argv)");
}

Reply Client::command(const std::vector<std::string>& argv) {
  if (argv.empty()) throw InvalidInputError("command needs at least a verb");
  return exec(argv);
}

int64_t Client::exec_int(const std::vector<std::string>& argv) {
  Reply r = exec(argv);
  if (r.kind == ReplyKind::Int) return r.integer;
  if (r.kind == ReplyKind::Error) raise_reply_error(r);
  raise_unexpected(r);
}

int64_t Client::exec_count(const std::vector<std::string>& argv) {
  int64_t n = exec_int(argv);
  if (n < 0) throw ProtocolError("expected non-negative count, got " + i2s(n));
  return n;
}

void Client::exec_ok(const std::vector<std::string>& argv) {
  Reply r = exec(argv);
  if (r.kind == ReplyKind::Simple && r.str() == "OK") return;
  if (r.kind == ReplyKind::Error) raise_reply_error(r);
  raise_unexpected(r);
}

bool Client::exec_bool(const std::vector<std::string>& argv) {
  Reply r = exec(argv);
  if (r.kind == ReplyKind::Int) return r.integer == 1;
  if (r.kind == ReplyKind::Error) raise_reply_error(r);
  raise_unexpected(r);
}

std::vector<std::string> Client::exec_bulks(const std::vector<std::string>& argv) {
  Reply r = exec(argv);
  if (r.kind == ReplyKind::Array || r.kind == ReplyKind::Set) return detail::array_to_bulks(r);
  if (r.kind == ReplyKind::Error) raise_reply_error(r);
  raise_unexpected(r);
}

OptBytes Client::exec_opt_bulk(const std::vector<std::string>& argv) {
  Reply r = exec(argv);
  switch (r.kind) {
    case ReplyKind::Bulk: return r.bytes;
    case ReplyKind::Nil:
    case ReplyKind::Null: return std::nullopt;
    case ReplyKind::Error: raise_reply_error(r);
    default: raise_unexpected(r);
  }
}

// --- core string / generic key (§3.1) -----------------------------------

void Client::ping() {
  if (emb_) return;  // embedded always OK
  Reply r = exec({"PING"});
  if (r.kind == ReplyKind::Simple && r.str() == "PONG") return;
  if (r.kind == ReplyKind::Error) raise_reply_error(r);
  raise_unexpected(r);
}

void Client::set(std::string_view key, std::string_view value) {
  exec_ok({"SET", std::string(key), std::string(value)});
}

OptBytes Client::get(std::string_view key) { return exec_opt_bulk({"GET", std::string(key)}); }

int64_t Client::del(const ByteList& keys) { return exec_count(verb_list("DEL", keys)); }
int64_t Client::exists(const ByteList& keys) { return exec_count(verb_list("EXISTS", keys)); }
int64_t Client::incr(std::string_view key) { return exec_int({"INCR", std::string(key)}); }
int64_t Client::incr_by(std::string_view key, int64_t delta) {
  return exec_int({"INCRBY", std::string(key), i2s(delta)});
}

bool Client::expire(std::string_view key, Duration ttl) {
  int64_t ms = ttl.count() < 0 ? 0 : ttl.count();
  return exec_bool({"PEXPIRE", std::string(key), i2s(ms)});
}
bool Client::persist(std::string_view key) { return exec_bool({"PERSIST", std::string(key)}); }
int64_t Client::ttl_ms(std::string_view key) { return exec_int({"PTTL", std::string(key)}); }

std::string Client::type_of(std::string_view key) {
  Reply r = exec({"TYPE", std::string(key)});
  if (r.kind == ReplyKind::Simple) return r.str();
  if (r.kind == ReplyKind::Error) raise_reply_error(r);
  raise_unexpected(r);
}

int64_t Client::dbsize() { return exec_count({"DBSIZE"}); }
void Client::flushall() { exec_ok({"FLUSHALL"}); }

void Client::set_with_ttl(std::string_view key, std::string_view value, Duration ttl) {
  int64_t ms = ttl.count() < 0 ? 0 : ttl.count();
  exec_ok({"SET", std::string(key), std::string(value), "PX", i2s(ms)});
}

std::vector<OptBytes> Client::mget(const ByteList& keys) {
  Reply r = exec(verb_list("MGET", keys));
  if (r.kind == ReplyKind::Error) raise_reply_error(r);
  if (r.kind != ReplyKind::Array) raise_unexpected(r);
  std::vector<OptBytes> out;
  out.reserve(r.array.size());
  for (const auto& it : r.array) {
    if (it.kind == ReplyKind::Bulk) out.push_back(it.bytes);
    else if (it.is_nil()) out.push_back(std::nullopt);
    else raise_unexpected(it);
  }
  return out;
}

void Client::mset(const std::vector<std::pair<std::string_view, std::string_view>>& pairs) {
  std::vector<std::string> argv;
  argv.reserve(pairs.size() * 2 + 1);
  argv.emplace_back("MSET");
  for (auto& [k, v] : pairs) {
    argv.emplace_back(k);
    argv.emplace_back(v);
  }
  exec_ok(argv);
}

int64_t Client::publish(std::string_view channel, std::string_view message) {
  return exec_count({"PUBLISH", std::string(channel), std::string(message)});
}

}  // namespace kevy
