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

// ── push model ─────────────────────────────────────────────────────────
//
// kevy-ffi is poll-only. To turn that into a JS-side push we spawn a native
// thread that spins kevy_sub_next; each frame we invoke the JS callback,
// which Nitro converts (void return => AsyncJSCallback) into a fire-and-
// forget hop onto the JS thread via the CallInvoker. The native thread
// never blocks on JS. On empty we yield — a busy poll, honest caveat: this
// burns a core while subscribed; a real engine sub_wait() would remove it.

void HybridKevyNitro::subscribePush(
    const std::string& channel,
    const std::function<void(const std::shared_ptr<ArrayBuffer>&)>& onMessage) {
  stopPushInternal();
  _pushSub = kevy_subscribe(
      _db, reinterpret_cast<const uint8_t*>(channel.data()), channel.size());
  _pollRunning.store(true, std::memory_order_release);
  KevySub* sub = _pushSub;
  auto cb = onMessage;
  _poller = std::thread([this, sub, cb]() {
    while (_pollRunning.load(std::memory_order_acquire)) {
      KevyBuf out{};
      if (kevy_sub_next(sub, &out) == 1) {
        std::vector<uint8_t> v(out.ptr, out.ptr + out.len);
        kevy_buf_free(out.ptr, out.len, out.cap);
        cb(ArrayBuffer::move(std::move(v))); // hops to the JS thread
      } else {
        std::this_thread::yield();
      }
    }
  });
}

void HybridKevyNitro::subscribePushBatched(
    const std::string& channel,
    const std::function<void(const std::vector<std::shared_ptr<ArrayBuffer>>&)>& onBatch) {
  stopPushInternal();
  _pushSub = kevy_subscribe(
      _db, reinterpret_cast<const uint8_t*>(channel.data()), channel.size());
  _pollRunning.store(true, std::memory_order_release);
  KevySub* sub = _pushSub;
  auto cb = onBatch;
  _poller = std::thread([this, sub, cb]() {
    while (_pollRunning.load(std::memory_order_acquire)) {
      std::vector<std::shared_ptr<ArrayBuffer>> batch;
      KevyBuf out{};
      while (kevy_sub_next(sub, &out) == 1) {
        std::vector<uint8_t> v(out.ptr, out.ptr + out.len);
        kevy_buf_free(out.ptr, out.len, out.cap);
        batch.push_back(ArrayBuffer::move(std::move(v)));
      }
      if (!batch.empty()) {
        cb(batch); // one hop for the whole drained batch
      } else {
        std::this_thread::yield();
      }
    }
  });
}

void HybridKevyNitro::stopPush() { stopPushInternal(); }

void HybridKevyNitro::stopPushInternal() {
  _pollRunning.store(false, std::memory_order_release);
  if (_poller.joinable()) {
    _poller.join();
  }
  if (_pushSub != nullptr) {
    kevy_sub_close(_pushSub);
    _pushSub = nullptr;
  }
}

} // namespace margelo::nitro::kevy
