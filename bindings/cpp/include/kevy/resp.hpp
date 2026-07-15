// kevy/resp.hpp — RESP2/RESP3 wire codec (contract §4.1).
//
// A recursive parser with "need more bytes" semantics so it works both over
// the self-contained FFI reply buffer and a streaming TCP socket, plus a
// multibulk request encoder. Fuzz-hardened: header-declared counts are capped
// by the bytes remaining, matching the Rust reference parser.
#ifndef KEVY_RESP_HPP
#define KEVY_RESP_HPP

#include <cstddef>
#include <cstdint>
#include <string>
#include <vector>

#include "kevy/reply.hpp"

namespace kevy {
namespace resp {

// Encode argv as a RESP multibulk request, appending to out.
void encode_command(std::string& out, const std::vector<std::string>& argv);

// Parse exactly one reply from the front of [buf, buf+len). Returns bytes
// consumed and fills out; returns 0 when more bytes are needed (out
// untouched). Throws ProtocolError on a malformed frame. Attribute frames
// (|N) are transparently skipped.
size_t parse_reply(const uint8_t* buf, size_t len, Reply& out);

// Decode one complete reply from a self-contained buffer (the FFI path).
// Throws ProtocolError on truncation/malformation.
Reply decode_reply(const uint8_t* buf, size_t len);

// Re-encode a decoded Reply back to RESP wire bytes (used by the C façade's
// raw command path so C callers can re-parse with any RESP parser).
void encode_reply(std::string& out, const Reply& r);

}  // namespace resp
}  // namespace kevy

#endif  // KEVY_RESP_HPP
