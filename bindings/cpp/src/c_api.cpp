#include "kevy/c_api.h"

#include <cstdlib>
#include <cstring>
#include <new>
#include <string>
#include <vector>

#include "kevy/client.hpp"
#include "kevy/errors.hpp"
#include "kevy/resp.hpp"

// The C façade wraps the C++ Client. It never lets an exception cross the
// extern "C" boundary (undefined behaviour); every entry point maps a thrown
// KevyError to a status code and stashes the text for kevy_client_last_error.

struct KevyClient {
  kevy::Client client;
  std::string last_error;
  explicit KevyClient(kevy::Client&& c) : client(std::move(c)) {}
};

namespace {

int status_of(const kevy::KevyError& e) {
  switch (e.kind()) {
    case kevy::ErrorKind::Store: return KEVY_STATUS_STORE;
    case kevy::ErrorKind::Io: return KEVY_STATUS_IO;
    case kevy::ErrorKind::Protocol: return KEVY_STATUS_PROTOCOL;
    case kevy::ErrorKind::ReadOnly: return KEVY_STATUS_READONLY;
    case kevy::ErrorKind::InvalidInput: return KEVY_STATUS_INVALID_INPUT;
    case kevy::ErrorKind::NotFound: return KEVY_STATUS_NOT_FOUND;
    case kevy::ErrorKind::Unsupported: return KEVY_STATUS_UNSUPPORTED;
    case kevy::ErrorKind::TimedOut: return KEVY_STATUS_TIMED_OUT;
    case kevy::ErrorKind::Closed: return KEVY_STATUS_CLOSED;
  }
  return KEVY_STATUS_PROTOCOL;
}

KevyBytes to_bytes(const std::string& s) {
  KevyBytes b{nullptr, 0};
  if (!s.empty()) {
    b.ptr = static_cast<char*>(std::malloc(s.size()));
    if (b.ptr != nullptr) {
      std::memcpy(b.ptr, s.data(), s.size());
      b.len = s.size();
    }
  }
  return b;
}

std::vector<std::string> to_argv(size_t argc, const char* const* argv, const size_t* argv_len) {
  std::vector<std::string> out;
  out.reserve(argc);
  for (size_t i = 0; i < argc; i++) out.emplace_back(argv[i], argv_len[i]);
  return out;
}

}  // namespace

extern "C" {

int kevy_client_connect(const char* url, KevyClient** out) {
  if (url == nullptr || out == nullptr) return KEVY_STATUS_MISUSE;
  try {
    auto* c = new KevyClient(kevy::Client::connect(url));
    *out = c;
    return KEVY_STATUS_OK;
  } catch (const kevy::KevyError& e) {
    return status_of(e);
  } catch (...) {
    return KEVY_STATUS_MISUSE;
  }
}

void kevy_client_close(KevyClient* c) { delete c; }

int kevy_client_command(KevyClient* c, size_t argc, const char* const* argv, const size_t* argv_len,
                        KevyBytes* out) {
  if (c == nullptr || out == nullptr || argc == 0) return KEVY_STATUS_MISUSE;
  try {
    kevy::Reply r = c->client.command(to_argv(argc, argv, argv_len));
    std::string wire;
    kevy::resp::encode_reply(wire, r);
    *out = to_bytes(wire);
    return KEVY_STATUS_OK;
  } catch (const kevy::KevyError& e) {
    c->last_error = e.text();
    return status_of(e);
  } catch (...) {
    c->last_error = "unknown error";
    return KEVY_STATUS_MISUSE;
  }
}

int kevy_client_get(KevyClient* c, const char* key, size_t key_len, KevyBytes* out, int* found) {
  if (c == nullptr || out == nullptr || found == nullptr) return KEVY_STATUS_MISUSE;
  try {
    auto v = c->client.get(std::string_view(key, key_len));
    if (v.has_value()) {
      *out = to_bytes(*v);
      *found = 1;
    } else {
      *out = KevyBytes{nullptr, 0};
      *found = 0;
    }
    return KEVY_STATUS_OK;
  } catch (const kevy::KevyError& e) {
    c->last_error = e.text();
    return status_of(e);
  } catch (...) {
    c->last_error = "unknown error";
    return KEVY_STATUS_MISUSE;
  }
}

int kevy_client_set(KevyClient* c, const char* key, size_t key_len, const char* val, size_t val_len,
                    unsigned long long ttl_ms) {
  if (c == nullptr) return KEVY_STATUS_MISUSE;
  try {
    std::string_view k(key, key_len), v(val, val_len);
    if (ttl_ms == 0) c->client.set(k, v);
    else c->client.set_with_ttl(k, v, std::chrono::milliseconds(ttl_ms));
    return KEVY_STATUS_OK;
  } catch (const kevy::KevyError& e) {
    c->last_error = e.text();
    return status_of(e);
  } catch (...) {
    c->last_error = "unknown error";
    return KEVY_STATUS_MISUSE;
  }
}

const char* kevy_client_last_error(KevyClient* c) {
  return c == nullptr ? "" : c->last_error.c_str();
}

void kevy_client_free_bytes(KevyBytes b) { std::free(b.ptr); }

}  // extern "C"
