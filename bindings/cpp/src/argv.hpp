// argv.hpp — small inline helpers for building RESP argv vectors from typed
// client parameters. Internal to the library.
#ifndef KEVY_INTERNAL_ARGV_HPP
#define KEVY_INTERNAL_ARGV_HPP

#include <string>
#include <vector>

#include "kevy/client.hpp"

namespace kevy {
namespace detail {

// Append every element of a ByteList as an owned argv entry.
inline void push_all(std::vector<std::string>& argv, const ByteList& xs) {
  for (auto x : xs) argv.emplace_back(x);
}

// "verb" then a key then a list — the shape of DEL-like/HDEL-like verbs.
inline std::vector<std::string> verb_key_list(const char* verb, std::string_view key,
                                              const ByteList& rest) {
  std::vector<std::string> argv;
  argv.reserve(rest.size() + 2);
  argv.emplace_back(verb);
  argv.emplace_back(key);
  push_all(argv, rest);
  return argv;
}

// "verb" then a list — the shape of DEL/EXISTS/MGET/SINTER.
inline std::vector<std::string> verb_list(const char* verb, const ByteList& rest) {
  std::vector<std::string> argv;
  argv.reserve(rest.size() + 1);
  argv.emplace_back(verb);
  push_all(argv, rest);
  return argv;
}

inline std::string i2s(int64_t n) { return std::to_string(n); }
inline std::string u2s(uint64_t n) { return std::to_string(n); }

}  // namespace detail
}  // namespace kevy

#endif  // KEVY_INTERNAL_ARGV_HPP
