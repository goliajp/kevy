#include <chrono>
#include <thread>

#include "argv.hpp"
#include "kevy/client.hpp"
#include "kevy/errors.hpp"
#include "reply_util.hpp"

// Blocking pops: BLPOP / BRPOP / BZPOPMIN (contract §3.14). Remote parks the
// server connection; embedded (pure-FFI, no blocking-pop symbol) EMULATES the
// block by polling the non-blocking pop on a short interval — observably
// correct, only a bounded poll latency differs. timeout == nullopt waits
// forever (wire 0); Some(zero) is rejected (ambiguous). Timeout is sent as
// fractional seconds.

namespace kevy {

using detail::raise_reply_error;
using detail::raise_unexpected;

namespace {

void check_args(const ByteList& keys, const std::optional<Duration>& timeout) {
  if (keys.empty()) throw InvalidInputError("blocking pop needs at least one key");
  if (timeout.has_value() && timeout->count() == 0)
    throw InvalidInputError("timeout Some(0) is ambiguous (wire 0 = wait forever); use nullopt to wait forever");
}

std::vector<std::string> blocking_argv(const char* verb, const ByteList& keys,
                                       const std::optional<Duration>& timeout) {
  std::vector<std::string> argv;
  argv.reserve(keys.size() + 2);
  argv.emplace_back(verb);
  detail::push_all(argv, keys);
  if (!timeout.has_value()) argv.emplace_back("0");
  else argv.push_back(detail::format_double(static_cast<double>(timeout->count()) / 1000.0));
  return argv;
}

// Poll deadline helpers for the embedded emulation.
using Clock = std::chrono::steady_clock;

bool block_expired(const std::optional<Clock::time_point>& deadline) {
  if (!deadline.has_value()) {
    std::this_thread::sleep_for(std::chrono::milliseconds(5));
    return false;
  }
  if (Clock::now() >= *deadline) return true;
  std::this_thread::sleep_for(std::chrono::milliseconds(5));
  return false;
}

std::optional<Clock::time_point> block_deadline(const std::optional<Duration>& timeout) {
  if (!timeout.has_value()) return std::nullopt;
  return Clock::now() + *timeout;
}

}  // namespace

std::optional<KV> Client::blpop(const ByteList& keys, std::optional<Duration> timeout) {
  check_args(keys, timeout);
  if (emb_) {
    auto deadline = block_deadline(timeout);
    for (;;) {
      for (auto k : keys) {
        Reply r = exec({"LPOP", std::string(k), "1"});
        if (r.kind == ReplyKind::Array && !r.array.empty() && r.array[0].kind == ReplyKind::Bulk)
          return KV{std::string(k), r.array[0].bytes};
        if (r.kind == ReplyKind::Bulk) return KV{std::string(k), r.bytes};
      }
      if (block_expired(deadline)) return std::nullopt;
    }
  }
  Reply r = exec(blocking_argv("BLPOP", keys, timeout));
  if (r.kind == ReplyKind::Array && r.array.size() == 2 && r.array[0].kind == ReplyKind::Bulk &&
      r.array[1].kind == ReplyKind::Bulk)
    return KV{r.array[0].bytes, r.array[1].bytes};
  if (r.is_nil()) return std::nullopt;
  if (r.kind == ReplyKind::Error) raise_reply_error(r);
  raise_unexpected(r);
}

std::optional<KV> Client::brpop(const ByteList& keys, std::optional<Duration> timeout) {
  check_args(keys, timeout);
  if (emb_) {
    auto deadline = block_deadline(timeout);
    for (;;) {
      for (auto k : keys) {
        Reply r = exec({"RPOP", std::string(k), "1"});
        if (r.kind == ReplyKind::Array && !r.array.empty() && r.array[0].kind == ReplyKind::Bulk)
          return KV{std::string(k), r.array[0].bytes};
        if (r.kind == ReplyKind::Bulk) return KV{std::string(k), r.bytes};
      }
      if (block_expired(deadline)) return std::nullopt;
    }
  }
  Reply r = exec(blocking_argv("BRPOP", keys, timeout));
  if (r.kind == ReplyKind::Array && r.array.size() == 2 && r.array[0].kind == ReplyKind::Bulk &&
      r.array[1].kind == ReplyKind::Bulk)
    return KV{r.array[0].bytes, r.array[1].bytes};
  if (r.is_nil()) return std::nullopt;
  if (r.kind == ReplyKind::Error) raise_reply_error(r);
  raise_unexpected(r);
}

std::optional<ZPopHit> Client::bzpopmin(const ByteList& keys, std::optional<Duration> timeout) {
  check_args(keys, timeout);
  if (emb_) {
    auto deadline = block_deadline(timeout);
    for (;;) {
      for (auto k : keys) {
        Reply r = exec({"ZPOPMIN", std::string(k), "1"});
        if (r.kind == ReplyKind::Array && r.array.size() >= 2 && r.array[0].kind == ReplyKind::Bulk)
          return ZPopHit{std::string(k), r.array[0].bytes, detail::score_of(r.array[1])};
      }
      if (block_expired(deadline)) return std::nullopt;
    }
  }
  Reply r = exec(blocking_argv("BZPOPMIN", keys, timeout));
  if (r.kind == ReplyKind::Array && r.array.size() == 3 && r.array[0].kind == ReplyKind::Bulk &&
      r.array[1].kind == ReplyKind::Bulk)
    return ZPopHit{r.array[0].bytes, r.array[1].bytes, detail::score_of(r.array[2])};
  if (r.is_nil()) return std::nullopt;
  if (r.kind == ReplyKind::Error) raise_reply_error(r);
  raise_unexpected(r);
}

}  // namespace kevy
