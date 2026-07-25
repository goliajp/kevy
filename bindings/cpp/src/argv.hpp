// argv.hpp — the internal command-argument vector (Args) and the small
// verb+list builders shared across the client translation units.
//
// Neither backend needs owned argument strings: the embedded path rebuilds
// (ptr, len) from each view and the remote path RESP-encodes them. So Args
// borrows every key/value/member as a std::string_view and only OWNS the few
// synthesized arguments (integers, formatted doubles) that have no caller
// storage to point at — those live in a std::deque scratch arena whose nodes
// never move, so appending one never dangles an earlier view.
#ifndef KEVY_INTERNAL_ARGV_HPP
#define KEVY_INTERNAL_ARGV_HPP

#include <cstdint>
#include <deque>
#include <initializer_list>
#include <string>
#include <string_view>
#include <vector>

#include "kevy/client.hpp"  // ByteList
#include "reply_util.hpp"   // format_double

namespace kevy {
namespace detail {

// A borrowed command argv with an owned-scratch tail. Borrowed views must
// outlive the Args; the scratch arena keeps synthesized args alive for its
// lifetime. Move-only — a move transfers the deque nodes and the view vector
// by pointer, so every view stays valid.
class Args {
 public:
  Args() = default;

  // Borrow each element. For inline call sites where every argument is a
  // literal, a string_view parameter, or a full-expression temporary (an
  // i2s()/u2s() result) that outlives the call it is passed into.
  Args(std::initializer_list<std::string_view> parts) : views_(parts) {}

  // Borrow each element of an owned argv the caller keeps alive (command()).
  explicit Args(const std::vector<std::string>& owned) : views_(owned.begin(), owned.end()) {}

  Args(const Args&) = delete;
  Args& operator=(const Args&) = delete;
  Args(Args&&) = default;
  Args& operator=(Args&&) = default;

  Args& add(std::string_view s) {
    views_.push_back(s);
    return *this;
  }
  Args& add_all(const ByteList& xs) {
    for (auto x : xs) views_.push_back(x);
    return *this;
  }
  // Synthesized arguments: owned by the scratch arena so the view is stable.
  Args& add_owned(std::string s) {
    views_.push_back(scratch_.emplace_back(std::move(s)));
    return *this;
  }
  Args& add_int(int64_t n) { return add_owned(std::to_string(n)); }
  Args& add_uint(uint64_t n) { return add_owned(std::to_string(n)); }
  Args& add_double(double d) { return add_owned(format_double(d)); }

  void reserve(size_t n) { views_.reserve(n); }
  const std::vector<std::string_view>& views() const { return views_; }
  size_t size() const { return views_.size(); }
  bool empty() const { return views_.empty(); }

 private:
  std::vector<std::string_view> views_;
  std::deque<std::string> scratch_;
};

// "verb" then a key then a list — DEL-like/HDEL-like verbs.
inline Args verb_key_list(std::string_view verb, std::string_view key, const ByteList& rest) {
  Args a;
  a.reserve(rest.size() + 2);
  a.add(verb).add(key).add_all(rest);
  return a;
}

// "verb" then a list — DEL/EXISTS/MGET/SINTER.
inline Args verb_list(std::string_view verb, const ByteList& rest) {
  Args a;
  a.reserve(rest.size() + 1);
  a.add(verb).add_all(rest);
  return a;
}

inline std::string i2s(int64_t n) { return std::to_string(n); }
inline std::string u2s(uint64_t n) { return std::to_string(n); }

}  // namespace detail
}  // namespace kevy

#endif  // KEVY_INTERNAL_ARGV_HPP
