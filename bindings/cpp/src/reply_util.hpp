// reply_util.hpp — shared reply→typed-value / reply→error helpers used across
// the client translation units (contract §2.2, §6).
#ifndef KEVY_INTERNAL_REPLY_UTIL_HPP
#define KEVY_INTERNAL_REPLY_UTIL_HPP

#include <string>
#include <vector>

#include "kevy/errors.hpp"
#include "kevy/reply.hpp"

namespace kevy {
namespace detail {

// Map a server error frame's text onto the structured taxonomy. Sets
// recognized=true and returns the StoreErrorKind for a known store-semantic
// error (WRONGTYPE, "is not an integer", …); otherwise recognized=false.
StoreErrorKind classify_store_error(const std::string& text, bool& recognized);

// Throw the right error for a Reply::Error: a recognized store-semantic error
// as StoreError (§6), any other wire text verbatim as ProtocolError (§2.2).
[[noreturn]] void raise_reply_error(const Reply& r);

// Throw a ProtocolError naming the actual reply variant (shape mismatch).
[[noreturn]] void raise_unexpected(const Reply& r);

// Turn an Array/Set reply of bulk/nil into a list, nil → empty string entries
// mapped by the caller's optional handling (here: empty string placeholder is
// avoided — callers wanting optionals use their own path).
std::vector<std::string> array_to_bulks(const Reply& r);

double score_of(const Reply& r);
double parse_float_bytes(const std::string& b);
uint64_t parse_uint_bytes(const std::string& b);

// Shortest round-trippable decimal for a double (matches Go's
// FormatFloat(f, 'g', -1, 64), used for scores / weights / timeouts).
std::string format_double(double f);

}  // namespace detail
}  // namespace kevy

#endif  // KEVY_INTERNAL_REPLY_UTIL_HPP
