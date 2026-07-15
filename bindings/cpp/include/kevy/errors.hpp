// kevy/errors.hpp — the canonical error taxonomy (contract §2).
//
// The C++ port maps the Rust reference's errors-as-values onto a
// `KevyError` exception hierarchy (the C++/Java/Python/C# idiom, §2.1). One
// subclass per variant so callers can `catch (const StoreError&)` OR inspect
// `err.kind()` / `err.store_error()` — the variant identity that the
// conformance suite (§6) asserts on survives both ways.
//
// A recognized store-semantic reply (WRONGTYPE, "not an integer", …) becomes
// a StoreError with a structured `StoreErrorKind`; any other `-ERR` becomes a
// ProtocolError with the wire text preserved verbatim (§2.2).
#ifndef KEVY_ERRORS_HPP
#define KEVY_ERRORS_HPP

#include <stdexcept>
#include <string>

namespace kevy {

// The canonical error taxonomy (contract §2.2).
enum class ErrorKind {
  Store,         // structured store-semantic error; see StoreErrorKind
  Io,            // OS/transport failure (file, socket, AOF)
  Protocol,      // RESP -ERR reply (verbatim) or malformed/unexpected shape
  ReadOnly,      // write rejected: target is a read-only replica
  InvalidInput,  // bad argument, rejected before touching state
  NotFound,      // a named object (index, view, key) does not exist
  Unsupported,   // op unavailable on this backend/build (IDX.* embedded, TLS)
  TimedOut,      // a bounded blocking call ran out its timeout
  Closed,        // the connection / in-process bus is gone (EOF)
};

// Structured store-semantic error carried by a StoreError (contract §2.3).
enum class StoreErrorKind {
  None,         // not a store error
  WrongType,    // key holds a different type than the command expects
  NotInteger,   // value is not a base-10 integer (INCR family)
  Overflow,     // result would overflow i64
  OutOfRange,   // index outside the collection (LSET)
  NoSuchKey,    // key does not exist where the command requires one (LSET)
  NotFloat,     // value is not a valid float (INCRBYFLOAT)
  OutOfMemory,  // maxmemory exceeded under NoEviction (Redis OOM)
};

const char* to_string(StoreErrorKind s);
const char* to_string(ErrorKind k);

// Base class of every kevy client error. Inspect kind()/store_error(), or
// catch the concrete subclass below.
class KevyError : public std::runtime_error {
 public:
  KevyError(ErrorKind kind, std::string msg,
            StoreErrorKind store = StoreErrorKind::None)
      : std::runtime_error(msg), kind_(kind), store_(store), text_(std::move(msg)) {}

  ErrorKind kind() const noexcept { return kind_; }
  StoreErrorKind store_error() const noexcept { return store_; }
  // The wire/description text (for Protocol/InvalidInput/NotFound/Unsupported).
  const std::string& text() const noexcept { return text_; }

 private:
  ErrorKind kind_;
  StoreErrorKind store_;
  std::string text_;
};

// One subclass per variant (contract §2.1 — variant identity survives).
class StoreError : public KevyError {
 public:
  explicit StoreError(StoreErrorKind s)
      : KevyError(ErrorKind::Store, std::string("store error: ") + to_string(s), s) {}
};
class IoError : public KevyError {
 public:
  explicit IoError(std::string msg) : KevyError(ErrorKind::Io, std::move(msg)) {}
};
class ProtocolError : public KevyError {
 public:
  explicit ProtocolError(std::string msg) : KevyError(ErrorKind::Protocol, std::move(msg)) {}
};
class ReadOnlyError : public KevyError {
 public:
  ReadOnlyError() : KevyError(ErrorKind::ReadOnly, "READONLY You can't write against a read only replica") {}
};
class InvalidInputError : public KevyError {
 public:
  explicit InvalidInputError(std::string msg) : KevyError(ErrorKind::InvalidInput, std::move(msg)) {}
};
class NotFoundError : public KevyError {
 public:
  explicit NotFoundError(std::string msg) : KevyError(ErrorKind::NotFound, std::move(msg)) {}
};
class UnsupportedError : public KevyError {
 public:
  explicit UnsupportedError(std::string msg) : KevyError(ErrorKind::Unsupported, std::move(msg)) {}
};
class TimedOutError : public KevyError {
 public:
  TimedOutError() : KevyError(ErrorKind::TimedOut, "timed out") {}
};
class ClosedError : public KevyError {
 public:
  ClosedError() : KevyError(ErrorKind::Closed, "connection closed") {}
};

}  // namespace kevy

#endif  // KEVY_ERRORS_HPP
