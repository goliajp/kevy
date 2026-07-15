#include "HybridKevyNitro.hpp"

#include <cstdint>
#include <pthread.h>
#include <vector>

namespace margelo::nitro::kevy {

double HybridKevyNitro::abi() { return static_cast<double>(kevy_abi()); }

// Copy a kevy-owned KevyBuf into a JS-owned ArrayBuffer, then free the
// KevyBuf. One copy — unavoidable, the engine owns its Vec and JS owns the
// ArrayBuffer. The win is the *crossing* being cheap now, not this copy.
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
// kevy-ffi is poll-only for the drain (kevy_sub_next), but kevy_sub_wait
// parks the calling thread in the kernel on the engine's mpsc until a frame
// arrives (or timeout). We spawn one native thread that blocks in
// kevy_sub_wait and, per frame, invokes the JS callback — which Nitro turns
// (void return => AsyncJSCallback) into a fire-and-forget hop onto the JS
// thread via the CallInvoker. No busy-spin: idle costs zero CPU. We wait in
// 250 ms slices so stopPush stays responsive — the run flag is re-checked
// between waits, so join() returns within one slice.
static constexpr uint64_t kWaitSliceMs = 250;

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
    // Name the thread so it's distinct from the JS thread it was spawned
    // from (bionic otherwise inherits the creator's name, "mqt_v_js").
    pthread_setname_np(pthread_self(), "kevy-push-poll");
    while (_pollRunning.load(std::memory_order_acquire)) {
      KevyBuf out{};
      int32_t rc = kevy_sub_wait(sub, kWaitSliceMs, &out); // kernel park
      if (rc == 1) {
        std::vector<uint8_t> v(out.ptr, out.ptr + out.len);
        kevy_buf_free(out.ptr, out.len, out.cap);
        cb(ArrayBuffer::move(std::move(v))); // hops to the JS thread
      }
      // rc == 0: timeout — loop, re-check the run flag. rc < 0: closed.
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
    // Name the thread so it's distinct from the JS thread it was spawned
    // from (bionic otherwise inherits the creator's name, "mqt_v_js").
    pthread_setname_np(pthread_self(), "kevy-push-poll");
    while (_pollRunning.load(std::memory_order_acquire)) {
      KevyBuf out{};
      int32_t rc = kevy_sub_wait(sub, kWaitSliceMs, &out); // block for frame 1
      if (rc != 1) {
        continue; // timeout or closed — re-check the run flag
      }
      std::vector<std::shared_ptr<ArrayBuffer>> batch;
      std::vector<uint8_t> v(out.ptr, out.ptr + out.len);
      kevy_buf_free(out.ptr, out.len, out.cap);
      batch.push_back(ArrayBuffer::move(std::move(v)));
      // Drain everything else already queued — non-blocking — so the whole
      // burst rides one JS hop.
      KevyBuf more{};
      while (kevy_sub_next(sub, &more) == 1) {
        std::vector<uint8_t> mv(more.ptr, more.ptr + more.len);
        kevy_buf_free(more.ptr, more.len, more.cap);
        batch.push_back(ArrayBuffer::move(std::move(mv)));
      }
      cb(batch); // one hop for the whole batch
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
