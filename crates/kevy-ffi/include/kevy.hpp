// kevy.hpp — header-only C++17 wrapper over the kevy C ABI (kevy.h).
//
// Thin by design: RAII around the two handle types, a std::string_view
// command interface, and a small RESP parser that turns replies into a
// Value tree. No exceptions on protocol errors — a RESP error is a Value
// like any other, because the engine answering "-ERR …" is a working
// engine, not a broken call. Exceptions are reserved for ABI misuse
// (null handles), which is a programming bug.
//
//   kevy::Db db("data/");                       // or Db::in_memory()
//   db.cmd({"SET", "k", "v"});
//   auto v = db.cmd({"GET", "k"});              // v.bulk() == "v"
//   auto sub = db.subscribe("room:42");
//   db.cmd({"PUBLISH", "room:42", "hi"});
//   while (auto f = sub.next()) { /* f->arr()[2].bulk() == "hi" */ }

#ifndef KEVY_HPP
#define KEVY_HPP

#include <cstdint>
#include <memory>
#include <optional>
#include <stdexcept>
#include <string>
#include <string_view>
#include <vector>

#include "kevy.h"

namespace kevy {

// One node of a parsed RESP reply.
class Value {
 public:
  enum class Kind { Simple, Error, Int, Bulk, Nil, Array };

  Kind kind() const { return kind_; }
  bool is_error() const { return kind_ == Kind::Error; }
  bool is_nil() const { return kind_ == Kind::Nil; }

  // Simple / Error / Bulk payload. Empty for other kinds.
  const std::string &bulk() const { return text_; }
  int64_t integer() const { return int_; }
  const std::vector<Value> &arr() const { return arr_; }

  static Value simple(std::string s) { return Value(Kind::Simple, std::move(s)); }
  static Value error(std::string s) { return Value(Kind::Error, std::move(s)); }
  static Value integer_of(int64_t v) {
    Value out(Kind::Int, {});
    out.int_ = v;
    return out;
  }
  static Value bulk_of(std::string s) { return Value(Kind::Bulk, std::move(s)); }
  static Value nil() { return Value(Kind::Nil, {}); }
  static Value array(std::vector<Value> items) {
    Value out(Kind::Array, {});
    out.arr_ = std::move(items);
    return out;
  }

 private:
  Value(Kind k, std::string t) : kind_(k), text_(std::move(t)) {}
  Kind kind_;
  std::string text_;
  int64_t int_ = 0;
  std::vector<Value> arr_;
};

namespace detail {

// Recursive-descent RESP parse over one complete reply. The engine always
// hands back a complete encoding, so truncation here is an ABI bug worth
// throwing about.
inline Value parse(const uint8_t *&p, const uint8_t *end) {
  auto line = [&]() -> std::string {
    const uint8_t *start = p;
    while (p + 1 < end && !(p[0] == '\r' && p[1] == '\n')) p++;
    if (p + 1 >= end) throw std::runtime_error("kevy: truncated RESP reply");
    std::string s(reinterpret_cast<const char *>(start), p - start);
    p += 2;
    return s;
  };
  if (p >= end) throw std::runtime_error("kevy: empty RESP reply");
  const char tag = static_cast<char>(*p++);
  switch (tag) {
    case '+': return Value::simple(line());
    case '-': return Value::error(line());
    case ':': return Value::integer_of(std::stoll(line()));
    case '$': {
      const long long n = std::stoll(line());
      if (n < 0) return Value::nil();
      if (end - p < n + 2) throw std::runtime_error("kevy: truncated bulk");
      std::string s(reinterpret_cast<const char *>(p), static_cast<size_t>(n));
      p += n + 2;
      return Value::bulk_of(std::move(s));
    }
    case '*': {
      const long long n = std::stoll(line());
      if (n < 0) return Value::nil();
      std::vector<Value> items;
      items.reserve(static_cast<size_t>(n));
      for (long long i = 0; i < n; i++) items.push_back(parse(p, end));
      return Value::array(std::move(items));
    }
    default: throw std::runtime_error("kevy: unknown RESP tag");
  }
}

inline Value take(KevyBuf buf) {
  const uint8_t *p = buf.ptr;
  const uint8_t *end = buf.ptr + buf.len;
  try {
    Value v = parse(p, end);
    kevy_buf_free(buf.ptr, buf.len, buf.cap);
    return v;
  } catch (...) {
    kevy_buf_free(buf.ptr, buf.len, buf.cap);
    throw;
  }
}

}  // namespace detail

// A subscription handle. Poll with next(); drop to unsubscribe.
class Subscription {
 public:
  Subscription(Subscription &&) = default;
  Subscription &operator=(Subscription &&) = default;

  // One pending frame (message / pmessage / ack), or nullopt when drained.
  std::optional<Value> next() {
    KevyBuf buf;
    const int32_t rc = kevy_sub_next(sub_.get(), &buf);
    if (rc < 0) throw std::runtime_error("kevy: subscription misuse");
    if (rc == 0) return std::nullopt;
    return detail::take(buf);
  }

 private:
  friend class Db;
  explicit Subscription(KevySub *s) : sub_(s) {}
  struct Close {
    void operator()(KevySub *s) const { kevy_sub_close(s); }
  };
  std::unique_ptr<KevySub, Close> sub_;
};

// The embedded engine. Open persistent or in-memory; drop to close.
class Db {
 public:
  explicit Db(std::string_view dir)
      : db_(kevy_open(reinterpret_cast<const uint8_t *>(dir.data()), dir.size())) {
    if (!db_) throw std::runtime_error("kevy: open failed");
  }

  static Db in_memory() { return Db(kevy_open_mem()); }

  // Run one command; argv[0] is the verb. A RESP -ERR comes back as a
  // Value with is_error() — inspect it, it is data.
  Value cmd(std::initializer_list<std::string_view> argv) {
    std::vector<const uint8_t *> ptrs;
    std::vector<size_t> lens;
    ptrs.reserve(argv.size());
    lens.reserve(argv.size());
    for (const auto &a : argv) {
      ptrs.push_back(reinterpret_cast<const uint8_t *>(a.data()));
      lens.push_back(a.size());
    }
    KevyBuf out;
    const int32_t rc = kevy_cmd(db_.get(), ptrs.size(), ptrs.data(), lens.data(), &out);
    if (rc != 0) throw std::runtime_error("kevy: kevy_cmd misuse");
    return detail::take(out);
  }

  Subscription subscribe(std::string_view chan) {
    KevySub *s = kevy_subscribe(
        db_.get(), reinterpret_cast<const uint8_t *>(chan.data()), chan.size());
    if (!s) throw std::runtime_error("kevy: subscribe failed");
    return Subscription(s);
  }

  Subscription psubscribe(std::string_view pattern) {
    KevySub *s = kevy_psubscribe(
        db_.get(), reinterpret_cast<const uint8_t *>(pattern.data()), pattern.size());
    if (!s) throw std::runtime_error("kevy: psubscribe failed");
    return Subscription(s);
  }

  static std::string_view version() { return kevy_version(); }

 private:
  explicit Db(KevyDb *raw) : db_(raw) {
    if (!db_) throw std::runtime_error("kevy: open failed");
  }
  struct Close {
    void operator()(KevyDb *d) const { kevy_close(d); }
  };
  std::unique_ptr<KevyDb, Close> db_;
};

}  // namespace kevy

#endif  // KEVY_HPP
