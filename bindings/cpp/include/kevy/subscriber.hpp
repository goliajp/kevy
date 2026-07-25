// kevy/subscriber.hpp — pub/sub consumer side (contract §3.11). A subscribed
// connection cannot send normal commands, so the consumer is a distinct type;
// publishing is done from a normal Client. Backends by URL: kevy://redis://
// tcp:// use a dedicated TCP socket; mem://<name> / file:// use the in-process
// bus. Anonymous mem:// is rejected (no other producer — use a named bus).
#ifndef KEVY_SUBSCRIBER_HPP
#define KEVY_SUBSCRIBER_HPP

#include <chrono>
#include <memory>
#include <optional>
#include <string>
#include <string_view>
#include <utility>
#include <vector>

#include "kevy/types.hpp"

namespace kevy {

namespace detail {
class RespConn;
struct EmbSub;
}  // namespace detail

using SubDuration = std::chrono::milliseconds;

class Subscriber {
 public:
  // Open a fresh subscriber without subscribing yet. Anonymous mem:// rejected.
  static Subscriber connect(const std::string& url);
  // Connect and subscribe in one step (≥1 channel required).
  static Subscriber connect_channels(const std::string& url, const std::vector<std::string_view>& channels);

  ~Subscriber();
  Subscriber(Subscriber&&) noexcept;
  Subscriber& operator=(Subscriber&&) noexcept;
  Subscriber(const Subscriber&) = delete;
  Subscriber& operator=(const Subscriber&) = delete;

  void subscribe(const std::vector<std::string_view>& channels);
  void psubscribe(const std::vector<std::string_view>& patterns);
  void unsubscribe(const std::vector<std::string_view>& channels);   // empty = all
  void punsubscribe(const std::vector<std::string_view>& patterns);  // empty = all

  // Negotiate RESP3 push frames; remote-only, must precede any subscribe.
  PubsubEvent hello3();

  // Block for the next frame (acks + deliveries). Close → ClosedError.
  PubsubEvent recv();
  // Block for the next Message/Pmessage, skipping ack frames.
  std::pair<std::string, std::string> recv_message();

  // Bound a blocking recv (nullopt clears it). Timeout surfaces as TimedOut.
  void set_read_timeout(std::optional<SubDuration> d);

  void close();

 private:
  Subscriber() = default;
  std::unique_ptr<detail::RespConn> remote_;
  std::unique_ptr<detail::EmbSub> emb_;
  std::optional<SubDuration> read_timeout_;
};

}  // namespace kevy

#endif  // KEVY_SUBSCRIBER_HPP
