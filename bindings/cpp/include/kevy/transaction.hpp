// kevy/transaction.hpp — MULTI / EXEC / DISCARD + WATCH (contract §3.12).
// Remote-only. A Transaction abandoned without exec/discard sends an implicit
// DISCARD in its destructor (RAII, §3.12) so the socket is never left in MULTI
// mode.
#ifndef KEVY_TRANSACTION_HPP
#define KEVY_TRANSACTION_HPP

#include <cstddef>
#include <optional>
#include <string>
#include <string_view>
#include <vector>

#include "kevy/reply.hpp"

namespace kevy {

namespace detail {
class RespConn;
class Args;
}

// A typed cursor over a successful EXEC's per-command replies (§4.10). Each
// next_* consumes one reply; a variant mismatch throws Protocol but the cursor
// still advances so expect_empty() stays meaningful.
class TransactionReplies {
 public:
  explicit TransactionReplies(std::vector<Reply> items) : items_(std::move(items)) {}

  size_t remaining() const { return items_.size() - pos_; }
  void expect_empty();  // throws if any replies remain (arity gate)
  Reply raw();
  void next_ok();                          // expects +OK
  bool next_ok_or_nil();                   // +OK → true, Nil → false
  int64_t next_int();
  std::optional<std::string> next_bulk();  // Bulk / Nil
  std::vector<std::optional<std::string>> next_array_of_bulks();  // MGET
  std::string next_simple();               // any simple string (+PONG)

 private:
  std::vector<Reply> items_;
  size_t pos_ = 0;
};

// One in-flight MULTI block over a remote connection.
class Transaction {
 public:
  explicit Transaction(detail::RespConn* c) : conn_(c), live_(true) {}
  ~Transaction();
  Transaction(Transaction&& o) noexcept : conn_(o.conn_), live_(o.live_) { o.live_ = false; }
  Transaction& operator=(Transaction&&) = delete;
  Transaction(const Transaction&) = delete;
  Transaction& operator=(const Transaction&) = delete;

  // Raw argv passthrough (expects +QUEUED).
  Transaction& queue(const std::vector<std::string>& parts);

  // Typed chainable builders (each expects +QUEUED).
  Transaction& set(std::string_view key, std::string_view value);
  Transaction& get(std::string_view key);
  Transaction& del(const std::vector<std::string_view>& keys);
  Transaction& exists(const std::vector<std::string_view>& keys);
  Transaction& incr(std::string_view key);
  Transaction& incr_by(std::string_view key, int64_t delta);
  Transaction& mget(const std::vector<std::string_view>& keys);
  Transaction& mset(const std::vector<std::pair<std::string_view, std::string_view>>& pairs);

  // Send EXEC and return the per-command replies. A WATCH abort (Nil)
  // collapses to an empty list (legacy).
  std::vector<Reply> exec();
  // exec but nullopt on a WATCH abort; a present (possibly empty) list = success.
  std::optional<std::vector<Reply>> exec_watched();
  // Typed cursor; a WATCH abort throws Protocol.
  TransactionReplies exec_typed();
  // Typed cursor; nullopt on WATCH abort.
  std::optional<TransactionReplies> exec_watched_typed();

  // Abandon the queued commands.
  void discard();
  // Send an implicit DISCARD if still live (idempotent).
  void close();

 private:
  void queue_argv(const detail::Args& argv);
  Reply finish_exec();
  detail::RespConn* conn_;
  bool live_;
};

}  // namespace kevy

#endif  // KEVY_TRANSACTION_HPP
