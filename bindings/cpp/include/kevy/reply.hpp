// kevy/reply.hpp — the universal decoded RESP value (contract §4.1).
//
// One `Reply` covers every RESP2 and RESP3 shape. A tagged struct (rather
// than std::variant) keeps the payload fields directly reachable and mirrors
// the other ports' Reply type field-for-field, so the conformance tests read
// the same on every language.
#ifndef KEVY_REPLY_HPP
#define KEVY_REPLY_HPP

#include <array>
#include <cstdint>
#include <string>
#include <utility>
#include <vector>

namespace kevy {

// Tags one node of a decoded RESP reply. RESP2 uses the first six; RESP3
// adds the rest (contract §4.1).
enum class ReplyKind {
  Simple,     // +OK
  Error,      // -ERR … (text without the leading '-')
  Int,        // :N
  Bulk,       // $len …
  Nil,        // $-1 / *-1 RESP2 null
  Array,      // *N
  Map,        // %N   (RESP3)
  Set,        // ~N   (RESP3)
  Double,     // ,    (RESP3)
  Boolean,    // #    (RESP3)
  Verbatim,   // =    (RESP3)
  BigNumber,  // (    (RESP3)
  Null,       // _    (RESP3 null)
  Push,       // >N   (RESP3 out-of-band, pub/sub delivery)
  BlobError,  // !    (RESP3)
};

// One decoded RESP value.
struct Reply {
  ReplyKind kind = ReplyKind::Nil;
  // Payload for Simple / Error / Bulk / BigNumber / BlobError, and the data
  // body for Verbatim. Binary-safe (may contain NUL).
  std::string bytes;
  int64_t integer = 0;             // Int
  double dbl = 0.0;                // Double
  bool boolean = false;           // Boolean
  std::array<char, 3> verbatim_fmt{{0, 0, 0}};  // Verbatim 3-char tag
  std::vector<Reply> array;        // Array / Set / Push
  std::vector<std::pair<Reply, Reply>> map;  // Map

  bool is_error() const { return kind == ReplyKind::Error || kind == ReplyKind::BlobError; }
  bool is_nil() const { return kind == ReplyKind::Nil || kind == ReplyKind::Null; }
  // Payload as a string (Simple/Error/Bulk/Verbatim/BigNumber/BlobError).
  const std::string& str() const { return bytes; }

  const char* shape() const;
};

}  // namespace kevy

#endif  // KEVY_REPLY_HPP
