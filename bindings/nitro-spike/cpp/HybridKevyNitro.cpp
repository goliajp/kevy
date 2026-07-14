#include "HybridKevyNitro.hpp"

#include <cstdint>
#include <vector>

namespace margelo::nitro::kevy {

double HybridKevyNitro::abi() { return static_cast<double>(kevy_abi()); }

// Copy a kevy-owned KevyBuf into a JS-owned ArrayBuffer, then free the
// KevyBuf. One copy — unavoidable, the engine owns its Vec and JS owns the
// ArrayBuffer. The win the spike measures is the *crossing*, not this copy.
static std::shared_ptr<ArrayBuffer> takeBuf(KevyBuf& buf) {
  std::vector<uint8_t> out(buf.ptr, buf.ptr + buf.len);
  kevy_buf_free(buf.ptr, buf.len, buf.cap);
  return ArrayBuffer::move(std::move(out));
}

std::shared_ptr<ArrayBuffer>
HybridKevyNitro::cmd(const std::shared_ptr<ArrayBuffer>& argv) {
  // The packed form: u32-LE length prefix per arg, then the bytes. Parse
  // in place — kevy_cmd copies internally, so the pointers may borrow the
  // input buffer for the duration of this synchronous call.
  const uint8_t* base = argv->data();
  size_t total = argv->size();
  std::vector<const uint8_t*> ptrs;
  std::vector<size_t> lens;
  size_t pos = 0;
  while (pos + 4 <= total) {
    uint32_t len = static_cast<uint32_t>(base[pos]) |
                   (static_cast<uint32_t>(base[pos + 1]) << 8) |
                   (static_cast<uint32_t>(base[pos + 2]) << 16) |
                   (static_cast<uint32_t>(base[pos + 3]) << 24);
    pos += 4;
    if (pos + len > total) {
      break;
    }
    ptrs.push_back(base + pos);
    lens.push_back(len);
    pos += len;
  }

  KevyBuf out{};
  kevy_cmd(_db, ptrs.size(), ptrs.data(), lens.data(), &out);
  return takeBuf(out);
}

void HybridKevyNitro::subscribe(const std::string& channel) {
  if (_sub != nullptr) {
    kevy_sub_close(_sub);
  }
  _sub = kevy_subscribe(
      _db, reinterpret_cast<const uint8_t*>(channel.data()), channel.size());
}

void HybridKevyNitro::publish(const std::string& channel,
                              const std::shared_ptr<ArrayBuffer>& payload) {
  const uint8_t* verb = reinterpret_cast<const uint8_t*>("PUBLISH");
  const uint8_t* chan = reinterpret_cast<const uint8_t*>(channel.data());
  const uint8_t* msg = payload->data();
  const uint8_t* ptrs[3] = {verb, chan, msg};
  size_t lens[3] = {7, channel.size(), payload->size()};
  KevyBuf out{};
  kevy_cmd(_db, 3, ptrs, lens, &out);
  kevy_buf_free(out.ptr, out.len, out.cap);
}

std::optional<std::shared_ptr<ArrayBuffer>> HybridKevyNitro::subNext() {
  if (_sub == nullptr) {
    return std::nullopt;
  }
  KevyBuf out{};
  int32_t rc = kevy_sub_next(_sub, &out);
  if (rc != 1) {
    return std::nullopt;
  }
  return takeBuf(out);
}

} // namespace margelo::nitro::kevy
