#include "kevy/embedded.hpp"

#include <vector>

#include "kevy.h"
#include "kevy/errors.hpp"
#include "kevy/resp.hpp"

namespace kevy {
namespace {

// Decode a kevy-owned KevyBuf into a Reply, then free it (exactly once).
Reply take_reply(KevyBuf buf) {
  if (buf.ptr == nullptr || buf.len == 0) {
    kevy_buf_free(buf.ptr, buf.len, buf.cap);
    throw ProtocolError("kevy: empty reply from embedded cmd");
  }
  try {
    Reply r = resp::decode_reply(buf.ptr, buf.len);
    kevy_buf_free(buf.ptr, buf.len, buf.cap);
    return r;
  } catch (...) {
    kevy_buf_free(buf.ptr, buf.len, buf.cap);
    throw;
  }
}

const uint8_t* byte_ptr(std::string_view s) {
  // The C ABI wants a non-null pointer even for length 0.
  static const uint8_t kEmpty = 0;
  return s.empty() ? &kEmpty : reinterpret_cast<const uint8_t*>(s.data());
}

}  // namespace

// --- Subscription -------------------------------------------------------

Subscription& Subscription::operator=(Subscription&& o) noexcept {
  if (this != &o) {
    close();
    sub_ = o.sub_;
    o.sub_ = nullptr;
  }
  return *this;
}

std::optional<Reply> Subscription::next() {
  if (sub_ == nullptr) throw ClosedError();
  KevyBuf out{};
  int32_t rc = kevy_sub_next(sub_, &out);
  if (rc < 0) throw ClosedError();
  if (rc == 0) return std::nullopt;
  return take_reply(out);
}

std::optional<Reply> Subscription::wait(uint64_t timeout_ms) {
  if (sub_ == nullptr) throw ClosedError();
  KevyBuf out{};
  int32_t rc = kevy_sub_wait(sub_, timeout_ms, &out);
  if (rc < 0) throw ClosedError();
  if (rc == 0) return std::nullopt;
  return take_reply(out);
}

void Subscription::close() {
  if (sub_ != nullptr) {
    kevy_sub_close(sub_);
    sub_ = nullptr;
  }
}

// --- EmbeddedStore ------------------------------------------------------

EmbeddedStore EmbeddedStore::open(std::string_view dir) {
  KevyDb* db = kevy_open(byte_ptr(dir), dir.size());
  if (db == nullptr) throw IoError("kevy: open failed");
  return EmbeddedStore(db);
}

EmbeddedStore EmbeddedStore::open_mem() {
  KevyDb* db = kevy_open_mem();
  if (db == nullptr) throw IoError("kevy: open_mem failed");
  return EmbeddedStore(db);
}

EmbeddedStore& EmbeddedStore::operator=(EmbeddedStore&& o) noexcept {
  if (this != &o) {
    close();
    db_ = o.db_;
    o.db_ = nullptr;
  }
  return *this;
}

void EmbeddedStore::close() {
  if (db_ != nullptr) {
    kevy_close(db_);
    db_ = nullptr;
  }
}

Reply EmbeddedStore::cmd(const std::vector<std::string_view>& argv) {
  if (db_ == nullptr) throw ClosedError();
  if (argv.empty()) throw InvalidInputError("kevy: empty argv");
  std::vector<const uint8_t*> ptrs(argv.size());
  std::vector<size_t> lens(argv.size());
  for (size_t i = 0; i < argv.size(); i++) {
    ptrs[i] = byte_ptr(argv[i]);
    lens[i] = argv[i].size();
  }
  KevyBuf out{};
  int32_t rc = kevy_cmd(db_, argv.size(), ptrs.data(), lens.data(), &out);
  if (rc != 0) throw ProtocolError("kevy: kevy_cmd misuse");
  return take_reply(out);
}

// --- SharedValue --------------------------------------------------------

SharedValue& SharedValue::operator=(SharedValue&& o) noexcept {
  if (this != &o) {
    release();
    ptr_ = o.ptr_;
    len_ = o.len_;
    cap_ = o.cap_;
    owns_ = o.owns_;
    o.owns_ = false;
    o.ptr_ = nullptr;
  }
  return *this;
}

void SharedValue::release() {
  if (owns_) {
    kevy_buf_free_shared(ptr_, len_, cap_);
    owns_ = false;
  }
}

// --- get / get_view -----------------------------------------------------

std::optional<SharedValue> EmbeddedStore::get_view(std::string_view key) {
  if (db_ == nullptr) throw ClosedError();
  KevyBuf out{};
  // Zero-copy shared lane: a bulk value is an Arc clone (refcount bump, no
  // byte copy) that SharedValue owns and views; the shared free drops the Arc
  // in its destructor.
  int32_t rc = kevy_get_shared(db_, byte_ptr(key), key.size(), &out);
  if (rc < 0) throw ProtocolError("kevy: kevy_get_shared misuse");
  if (rc == 0) return std::nullopt;
  return SharedValue(out.ptr, out.len, out.cap);
}

std::optional<std::string> EmbeddedStore::get(std::string_view key) {
  // The copying convenience over the shared lane; view() is empty (not a
  // dangling range) for a 0-length value, so std::string(view) is UB-free.
  std::optional<SharedValue> sv = get_view(key);
  if (!sv.has_value()) return std::nullopt;
  return std::string(sv->view());
}

void EmbeddedStore::set(std::string_view key, std::string_view value, uint64_t ttl_ms) {
  if (db_ == nullptr) throw ClosedError();
  int32_t rc = kevy_set(db_, byte_ptr(key), key.size(), byte_ptr(value), value.size(), ttl_ms);
  if (rc < 0) throw IoError("kevy: kevy_set misuse or storage error");
}

Subscription EmbeddedStore::subscribe(std::string_view channel) {
  if (db_ == nullptr) throw ClosedError();
  KevySub* s = kevy_subscribe(db_, byte_ptr(channel), channel.size());
  if (s == nullptr) throw IoError("kevy: subscribe failed");
  return Subscription(s);
}

Subscription EmbeddedStore::psubscribe(std::string_view pattern) {
  if (db_ == nullptr) throw ClosedError();
  KevySub* s = kevy_psubscribe(db_, byte_ptr(pattern), pattern.size());
  if (s == nullptr) throw IoError("kevy: psubscribe failed");
  return Subscription(s);
}

std::string EmbeddedStore::version() { return std::string(kevy_version()); }
uint32_t EmbeddedStore::abi() { return kevy_abi(); }

}  // namespace kevy
