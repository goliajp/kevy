#include "kevy/transaction.hpp"

#include "argv.hpp"
#include "kevy/client.hpp"
#include "kevy/errors.hpp"
#include "reply_util.hpp"
#include "resp_conn.hpp"

namespace kevy {

using detail::Args;
using detail::i2s;
using detail::raise_reply_error;
using detail::raise_unexpected;

namespace {

void simple_ok(const Reply& r) {
  if (r.kind == ReplyKind::Simple && r.str() == "OK") return;
  if (r.kind == ReplyKind::Error) raise_reply_error(r);
  raise_unexpected(r);
}

[[noreturn]] void tx_mismatch(const char* want, const Reply& got) {
  throw ProtocolError(std::string("transaction reply mismatch: expected ") + want + ", got " +
                      got.shape());
}

}  // namespace

// --- Client transaction entry points ------------------------------------

void Client::watch(const ByteList& keys) {
  if (keys.empty()) throw InvalidInputError("WATCH needs at least one key");
  detail::RespConn* conn = require_remote("WATCH");
  simple_ok(conn->request(detail::verb_list("WATCH", keys).views()));
}

void Client::unwatch() {
  detail::RespConn* conn = require_remote("UNWATCH");
  simple_ok(conn->request({"UNWATCH"}));
}

Transaction Client::multi() {
  detail::RespConn* conn = require_remote("MULTI/EXEC");
  simple_ok(conn->request({"MULTI"}));
  return Transaction(conn);
}

// --- Transaction --------------------------------------------------------

Transaction::~Transaction() { close(); }

void Transaction::queue_argv(const Args& argv) {
  Reply r = conn_->request(argv.views());
  if (r.kind == ReplyKind::Simple && r.str() == "QUEUED") return;
  if (r.kind == ReplyKind::Error) raise_reply_error(r);
  raise_unexpected(r);
}

Transaction& Transaction::queue(const std::vector<std::string>& parts) {
  if (parts.empty()) throw InvalidInputError("Transaction.queue needs at least a verb");
  queue_argv(Args(parts));
  return *this;
}

Transaction& Transaction::set(std::string_view key, std::string_view value) {
  queue_argv({"SET", key, value});
  return *this;
}
Transaction& Transaction::get(std::string_view key) {
  queue_argv({"GET", key});
  return *this;
}
Transaction& Transaction::del(const std::vector<std::string_view>& keys) {
  if (keys.empty()) throw InvalidInputError("Transaction.del needs at least one key");
  queue_argv(detail::verb_list("DEL", keys));
  return *this;
}
Transaction& Transaction::exists(const std::vector<std::string_view>& keys) {
  if (keys.empty()) throw InvalidInputError("Transaction.exists needs at least one key");
  queue_argv(detail::verb_list("EXISTS", keys));
  return *this;
}
Transaction& Transaction::incr(std::string_view key) {
  queue_argv({"INCR", key});
  return *this;
}
Transaction& Transaction::incr_by(std::string_view key, int64_t delta) {
  queue_argv({"INCRBY", key, i2s(delta)});
  return *this;
}
Transaction& Transaction::mget(const std::vector<std::string_view>& keys) {
  if (keys.empty()) throw InvalidInputError("Transaction.mget needs at least one key");
  queue_argv(detail::verb_list("MGET", keys));
  return *this;
}
Transaction& Transaction::mset(const std::vector<std::pair<std::string_view, std::string_view>>& pairs) {
  if (pairs.empty()) throw InvalidInputError("Transaction.mset needs a non-empty key/value list");
  Args argv;
  argv.reserve(pairs.size() * 2 + 1);
  argv.add("MSET");
  for (auto& [k, v] : pairs) argv.add(k).add(v);
  queue_argv(argv);
  return *this;
}

Reply Transaction::finish_exec() {
  live_ = false;
  return conn_->request({"EXEC"});
}

std::vector<Reply> Transaction::exec() {
  Reply r = finish_exec();
  if (r.kind == ReplyKind::Array) return std::move(r.array);
  if (r.is_nil()) return {};
  if (r.kind == ReplyKind::Error) raise_reply_error(r);
  raise_unexpected(r);
}

std::optional<std::vector<Reply>> Transaction::exec_watched() {
  Reply r = finish_exec();
  if (r.kind == ReplyKind::Array) return std::move(r.array);
  if (r.is_nil()) return std::nullopt;
  if (r.kind == ReplyKind::Error) raise_reply_error(r);
  raise_unexpected(r);
}

TransactionReplies Transaction::exec_typed() {
  Reply r = finish_exec();
  if (r.kind == ReplyKind::Array) return TransactionReplies(std::move(r.array));
  if (r.is_nil()) throw ProtocolError("transaction aborted by WATCH");
  if (r.kind == ReplyKind::Error) raise_reply_error(r);
  raise_unexpected(r);
}

std::optional<TransactionReplies> Transaction::exec_watched_typed() {
  Reply r = finish_exec();
  if (r.kind == ReplyKind::Array) return TransactionReplies(std::move(r.array));
  if (r.is_nil()) return std::nullopt;
  if (r.kind == ReplyKind::Error) raise_reply_error(r);
  raise_unexpected(r);
}

void Transaction::discard() {
  if (!live_) return;
  live_ = false;
  simple_ok(conn_->request({"DISCARD"}));
}

void Transaction::close() {
  if (live_) {
    live_ = false;
    try {
      conn_->request({"DISCARD"});
    } catch (...) {
      // best-effort on teardown
    }
  }
}

// --- TransactionReplies -------------------------------------------------

void TransactionReplies::expect_empty() {
  if (remaining() != 0)
    throw ProtocolError("transaction reply cursor has " + std::to_string(remaining()) +
                        " un-consumed replies");
}

Reply TransactionReplies::raw() {
  if (pos_ >= items_.size()) throw ProtocolError("transaction reply cursor exhausted");
  return std::move(items_[pos_++]);  // consumed once; the cursor never re-reads it
}

void TransactionReplies::next_ok() {
  Reply v = raw();
  if (v.kind == ReplyKind::Simple && v.str() == "OK") return;
  tx_mismatch("Simple(OK)", v);
}

bool TransactionReplies::next_ok_or_nil() {
  Reply v = raw();
  if (v.kind == ReplyKind::Simple && v.str() == "OK") return true;
  if (v.is_nil()) return false;
  tx_mismatch("Simple(OK) or Nil", v);
}

int64_t TransactionReplies::next_int() {
  Reply v = raw();
  if (v.kind == ReplyKind::Int) return v.integer;
  tx_mismatch("Int", v);
}

std::optional<std::string> TransactionReplies::next_bulk() {
  Reply v = raw();
  if (v.kind == ReplyKind::Bulk) return std::move(v.bytes);
  if (v.is_nil()) return std::nullopt;
  tx_mismatch("Bulk or Nil", v);
}

std::vector<std::optional<std::string>> TransactionReplies::next_array_of_bulks() {
  Reply v = raw();
  if (v.kind == ReplyKind::Array) {
    std::vector<std::optional<std::string>> out;
    out.reserve(v.array.size());
    for (auto& it : v.array) {
      if (it.kind == ReplyKind::Bulk) out.push_back(std::move(it.bytes));
      else if (it.is_nil()) out.push_back(std::nullopt);
      else tx_mismatch("Array element Bulk/Nil", it);
    }
    return out;
  }
  if (v.is_nil()) return {};
  tx_mismatch("Array", v);
}

std::string TransactionReplies::next_simple() {
  Reply v = raw();
  if (v.kind == ReplyKind::Simple) return std::move(v.bytes);
  tx_mismatch("Simple", v);
}

}  // namespace kevy
