// kevy/embedded.hpp — the in-process store the mem:// / file:// URLs use
// (contract §5). A thin C++ wrap of the kevy-ffi C ABI: one cmd(argv) path
// through which every verb is reachable, scalar get/set fast paths, and
// polled pub/sub. A protocol -ERR from cmd() is a Reply with is_error(), not
// a thrown error — the engine answering "no" is a working engine.
#ifndef KEVY_EMBEDDED_HPP
#define KEVY_EMBEDDED_HPP

#include <cstddef>
#include <cstdint>
#include <optional>
#include <span>
#include <string>
#include <string_view>
#include <vector>

#include "kevy/reply.hpp"

struct KevyDb;
struct KevySub;

namespace kevy {

// An RAII owner of a zero-copy GET result from the shared lane. Holds the
// engine's value handle (an Arc clone for a bulk value — no byte copy — or a
// single Vec for a small one) and exposes the bytes as a borrowed view. The
// view is valid for the SharedValue's lifetime; the handle is dropped exactly
// once in the destructor. Move-only. On a large GET this skips the value copy
// that get() makes into a std::string.
class SharedValue {
 public:
  SharedValue() = default;
  ~SharedValue() { release(); }
  SharedValue(SharedValue&& o) noexcept
      : ptr_(o.ptr_), len_(o.len_), cap_(o.cap_), owns_(o.owns_) {
    o.owns_ = false;
    o.ptr_ = nullptr;
  }
  SharedValue& operator=(SharedValue&& o) noexcept;
  SharedValue(const SharedValue&) = delete;
  SharedValue& operator=(const SharedValue&) = delete;

  // The value as a borrowed view (empty, not a dangling range, for a 0-length
  // value). Valid until this SharedValue is destroyed or moved from.
  std::string_view view() const {
    return len_ == 0 ? std::string_view()
                     : std::string_view(reinterpret_cast<const char*>(ptr_), len_);
  }
  std::span<const std::byte> bytes() const {
    return len_ == 0 ? std::span<const std::byte>()
                     : std::span<const std::byte>(reinterpret_cast<const std::byte*>(ptr_), len_);
  }
  const uint8_t* data() const { return ptr_; }
  size_t size() const { return len_; }
  bool empty() const { return len_ == 0; }

 private:
  friend class EmbeddedStore;
  SharedValue(uint8_t* ptr, size_t len, size_t cap) : ptr_(ptr), len_(len), cap_(cap), owns_(true) {}
  void release();

  uint8_t* ptr_ = nullptr;
  size_t len_ = 0;
  size_t cap_ = 0;   // opaque owner handle from the shared lane
  bool owns_ = false;
};

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
  // kevy's ~184 verbs is reachable here. Arguments are borrowed views (the C
  // ABI takes (ptr, len)); a -ERR is a Reply with is_error().
  Reply cmd(const std::vector<std::string_view>& argv);

  // Scalar fast GET (no argv/RESP framing). nullopt on miss/expired. Copies
  // the value into an owned std::string.
  std::optional<std::string> get(std::string_view key);
  // Zero-copy GET: the value comes back as a borrowed view held alive by the
  // returned SharedValue (no byte copy for a bulk value). nullopt on miss.
  // Prefer this over get() for large values you only read.
  std::optional<SharedValue> get_view(std::string_view key);
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
