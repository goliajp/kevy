// kevy/embedded.hpp — the in-process store the mem:// / file:// URLs use
// (contract §5). A thin C++ wrap of the kevy-ffi C ABI: one cmd(argv) path
// through which every verb is reachable, scalar get/set fast paths, and
// polled pub/sub. A protocol -ERR from cmd() is a Reply with is_error(), not
// a thrown error — the engine answering "no" is a working engine.
#ifndef KEVY_EMBEDDED_HPP
#define KEVY_EMBEDDED_HPP

#include <cstdint>
#include <optional>
#include <string>
#include <string_view>
#include <vector>

#include "kevy/reply.hpp"

struct KevyDb;
struct KevySub;

namespace kevy {

// One embedded subscription over a single channel or pattern (§5.2). Poll
// with next(), block with wait(); the higher-level multi-channel Subscriber
// (§3.11) is built on top of these.
class Subscription {
 public:
  explicit Subscription(KevySub* s) : sub_(s) {}
  ~Subscription() { close(); }
  Subscription(Subscription&& o) noexcept : sub_(o.sub_) { o.sub_ = nullptr; }
  Subscription& operator=(Subscription&& o) noexcept;
  Subscription(const Subscription&) = delete;
  Subscription& operator=(const Subscription&) = delete;

  // Poll one pending frame (message/pmessage/ack) without blocking.
  std::optional<Reply> next();
  // Block up to timeout_ms for one frame (0 = forever); nullopt on timeout.
  std::optional<Reply> wait(uint64_t timeout_ms);
  void close();
  KevySub* raw() const { return sub_; }

 private:
  KevySub* sub_;
};

// The embedded engine. Open persistent (open) or in-memory (open_mem); close
// exactly once. Every method is safe from multiple threads (the C ABI
// serialises internally).
class EmbeddedStore {
 public:
  static EmbeddedStore open(std::string_view dir);
  static EmbeddedStore open_mem();

  ~EmbeddedStore() { close(); }
  EmbeddedStore(EmbeddedStore&& o) noexcept : db_(o.db_) { o.db_ = nullptr; }
  EmbeddedStore& operator=(EmbeddedStore&& o) noexcept;
  EmbeddedStore(const EmbeddedStore&) = delete;
  EmbeddedStore& operator=(const EmbeddedStore&) = delete;

  // Run one command; argv[0] is the verb. The universal path — every one of
  // kevy's ~184 verbs is reachable here. A -ERR is a Reply with is_error().
  Reply cmd(const std::vector<std::string>& argv);

  // Scalar fast GET (no argv/RESP framing). nullopt on miss/expired.
  std::optional<std::string> get(std::string_view key);
  // Scalar fast SET (ttl_ms == 0 = no TTL).
  void set(std::string_view key, std::string_view value, uint64_t ttl_ms = 0);

  Subscription subscribe(std::string_view channel);
  Subscription psubscribe(std::string_view pattern);

  void close();
  bool valid() const { return db_ != nullptr; }
  KevyDb* raw() const { return db_; }

  static std::string version();
  static uint32_t abi();

 private:
  explicit EmbeddedStore(KevyDb* db) : db_(db) {}
  KevyDb* db_;
};

}  // namespace kevy

#endif  // KEVY_EMBEDDED_HPP
