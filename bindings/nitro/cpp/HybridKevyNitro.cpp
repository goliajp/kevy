#include "HybridKevyNitro.hpp"

#include <cstdint>
#include <pthread.h>
#include <stdexcept>
#include <string>
#include <utility>
#include <vector>

namespace margelo::nitro::kevy {

double HybridKevyNitro::abi() { return static_cast<double>(kevy_abi()); }

// Hand a kevy-owned KevyBuf to JS with ZERO binding-layer copy: wrap the
// engine's buffer directly in a JS ArrayBuffer and free it (kevy_buf_free)
// only when the JS side GCs the ArrayBuffer. Mirrors MMKV's MMKVManagedBuffer
// (jsi::ArrayBuffer over an owned MMBuffer). The previous takeBuf memcpy'd
// the KevyBuf into a std::vector first (an extra copy + alloc on every GET);
// wrap removes both from the hot path. The engine's own into_owned copy
// (its Vec lives behind the store lock) is the only remaining copy.
static std::shared_ptr<ArrayBuffer> takeBuf(KevyBuf& buf) {
  KevyBuf owned = buf; // POD (ptr/len/cap); the wrap owns it until GC.
  return ArrayBuffer::wrap(owned.ptr, owned.len, [owned]() {
    kevy_buf_free(owned.ptr, owned.len, owned.cap);
  });
}

// Same wrap, but for a buffer from the ZERO-COPY shared GET lane
// (kevy_get_shared): a bulk value's bytes are the engine's Arc, not a copy —
// the wrap holds it and drops the Arc via kevy_buf_free_shared at GC (cap is
// the opaque Arc owner handle). This is why big GET no longer memcpys N bytes.
static std::shared_ptr<ArrayBuffer> takeBufShared(KevyBuf& buf) {
  KevyBuf owned = buf;
  return ArrayBuffer::wrap(owned.ptr, owned.len, [owned]() {
    kevy_buf_free_shared(owned.ptr, owned.len, owned.cap);
  });
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
  // Most commands are a handful of args; reserve so the common case does no
  // per-call growth reallocs. A wider argv still grows (rare), and the reserve
  // never scales with a large *value* payload (it's an arg count, not bytes).
  ptrs.reserve(8);
  lens.reserve(8);
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
  kevy_cmd(_db.get(), ptrs.size(), ptrs.data(), lens.data(), &out);
  return takeBuf(out);
}

// ── scalar KV door ───────────────────────────────────────────────────────
//
// kevy_get / kevy_set directly: no argv, no RESP framing. The key/value come
// in as ArrayBuffers (borrowed for the synchronous call), the value goes out
// as one ArrayBuffer. getData uses the ZERO-COPY shared lane: a bulk value
// (>64 B) crosses as an Arc clone the JS buffer views directly — no byte copy,
// the analog of MMKV's mmap-page view. setData frames no reply at all.

std::optional<std::shared_ptr<ArrayBuffer>>
HybridKevyNitro::getData(const std::string& key) {
  KevyBuf out{};
  const auto* kp = reinterpret_cast<const uint8_t*>(key.data());
  int32_t rc = kevy_get_shared(_db.get(), kp, key.size(), &out);
  if (rc == 1) {
    return takeBufShared(out); // hit: zero-copy Arc view (the hot path)
  }
  if (rc == 0) {
    return std::nullopt; // miss — nothing to free
  }
  // rc < 0 = WRONGTYPE (get_shared's only error, a GET on a non-string key).
  // The shared lane can't convey it, so re-issue as a framed GET and let the
  // typed error surface — never a phantom miss. Only this rare case pays the
  // framed hop; hit/miss above never reach here.
  return getDataFramed(key);
}

// Parse a RESP GET reply from the framed lane into the scalar-door shape:
//   -ERR/-WRONGTYPE → throw (Nitro turns the std::exception into a typed JS
//                     error, message-carried, like every sibling door);
//   nil ($-1 / _)   → nullopt (miss);
//   bulk / simple   → copy the value out (rare path) and hand it to JS.
// Frees the reply buffer exactly once on every exit.
static std::optional<std::shared_ptr<ArrayBuffer>> parseGetReply(KevyBuf out) {
  const uint8_t* p = out.ptr;
  size_t n = out.len;
  if (p == nullptr || n == 0) {
    kevy_buf_free(out.ptr, out.len, out.cap);
    throw std::runtime_error("kevy: empty reply from framed GET");
  }
  uint8_t tag = p[0];
  if (tag == '-') { // error line: "-WRONGTYPE ...\r\n"
    size_t end = 1;
    while (end < n && p[end] != '\r') {
      end++;
    }
    std::string msg(reinterpret_cast<const char*>(p + 1), end - 1);
    kevy_buf_free(out.ptr, out.len, out.cap);
    throw std::runtime_error(msg);
  }
  if (tag == '_' || (tag == '$' && n >= 2 && p[1] == '-')) { // RESP3 / RESP2 nil
    kevy_buf_free(out.ptr, out.len, out.cap);
    return std::nullopt;
  }
  // Bulk "$<len>\r\n<value>\r\n" or Simple "+<value>\r\n": the value is the
  // bytes between the header line and the trailing CRLF (frame geometry — no
  // length re-parse). Copy it (rare path), then free the frame.
  size_t hdr = 1;
  while (hdr + 1 < n && !(p[hdr] == '\r' && p[hdr + 1] == '\n')) {
    hdr++;
  }
  size_t start = (tag == '+') ? 1 : hdr + 2;
  size_t stop = (tag == '+') ? hdr : (n >= 2 ? n - 2 : start);
  if (stop < start) {
    stop = start;
  }
  std::vector<uint8_t> value(p + start, p + stop);
  kevy_buf_free(out.ptr, out.len, out.cap);
  return ArrayBuffer::move(std::move(value));
}

std::optional<std::shared_ptr<ArrayBuffer>>
HybridKevyNitro::getDataFramed(const std::string& key) {
  const uint8_t* verb = reinterpret_cast<const uint8_t*>("GET");
  const uint8_t* kp = reinterpret_cast<const uint8_t*>(key.data());
  const uint8_t* ptrs[2] = {verb, kp};
  size_t lens[2] = {3, key.size()};
  KevyBuf out{};
  kevy_cmd(_db.get(), 2, ptrs, lens, &out);
  return parseGetReply(out);
}

void HybridKevyNitro::setData(const std::string& key,
                              const std::shared_ptr<ArrayBuffer>& value,
                              double ttlMs) {
  const auto* kp = reinterpret_cast<const uint8_t*>(key.data());
  kevy_set(_db.get(), kp, key.size(), value->data(), value->size(),
           static_cast<uint64_t>(ttlMs));
}

bool HybridKevyNitro::openAt(const std::string& dir) {
  // reset() closes the prior (in-memory ctor) db, then adopts the file-backed
  // one — the deleter runs on the old handle.
  _db.reset(kevy_open(reinterpret_cast<const uint8_t*>(dir.data()), dir.size()));
  return _db != nullptr; // NULL = open failed; caller must not use data ops
}

void HybridKevyNitro::subscribe(const std::string& channel) {
  _sub.reset(kevy_subscribe(
      _db.get(), reinterpret_cast<const uint8_t*>(channel.data()),
      channel.size()));
}

void HybridKevyNitro::publish(const std::string& channel,
                              const std::shared_ptr<ArrayBuffer>& payload) {
  const uint8_t* verb = reinterpret_cast<const uint8_t*>("PUBLISH");
  const uint8_t* chan = reinterpret_cast<const uint8_t*>(channel.data());
  const uint8_t* msg = payload->data();
  const uint8_t* ptrs[3] = {verb, chan, msg};
  size_t lens[3] = {7, channel.size(), payload->size()};
  KevyBuf out{};
  kevy_cmd(_db.get(), 3, ptrs, lens, &out);
  kevy_buf_free(out.ptr, out.len, out.cap);
}

std::optional<std::shared_ptr<ArrayBuffer>> HybridKevyNitro::subNext() {
  if (_sub == nullptr) {
    return std::nullopt;
  }
  KevyBuf out{};
  int32_t rc = kevy_sub_next(_sub.get(), &out);
  if (rc != 1) {
    return std::nullopt;
  }
  return takeBuf(out);
}

// ── push model ─────────────────────────────────────────────────────────
//
// The push family is a known-channel "give me payloads" API, so it drains
// the RESP-free lane: kevy_sub_wait_raw parks in the kernel until a delivery
// frame arrives (skipping subscribe/unsubscribe acks) and yields JUST the
// payload bytes — no `*3\r\n$7\r\nmessage…` framing to encode natively or
// parse in JS (the engine's encode_frame is ~208 ns/frame, measured). We
// spawn one native thread that blocks in kevy_sub_wait_raw and, per frame,
// invokes the JS callback — which Nitro turns (void return =>
// AsyncJSCallback) into a fire-and-forget hop onto the JS thread via the
// CallInvoker. No busy-spin: idle costs zero CPU. We wait in 250 ms slices so
// stopPush stays responsive — the run flag is re-checked between waits, so
// join() returns within one slice. (Callers that need the channel/kind or
// ack frames use the framed subscribe/subNext lane instead.)
static constexpr uint64_t kWaitSliceMs = 250;

// Name the poller thread so it's distinct from the JS thread it was spawned
// from (bionic otherwise inherits the creator's name, "mqt_v_js"). Darwin's
// pthread_setname_np names the *current* thread and takes one arg; bionic/Linux
// takes (thread, name). We're on the poller thread either way, so both name it.
static void namePollThread() {
#if defined(__APPLE__)
  pthread_setname_np("kevy-push-poll");
#else
  pthread_setname_np(pthread_self(), "kevy-push-poll");
#endif
}

void HybridKevyNitro::spawnPoller(const std::string& channel,
                                  std::function<void(KevySub*)> drain) {
  stopPushInternal();
  _pushSub.reset(kevy_subscribe(
      _db.get(), reinterpret_cast<const uint8_t*>(channel.data()),
      channel.size()));
  _pollRunning.store(true, std::memory_order_release);
  KevySub* sub = _pushSub.get();
  _poller = std::thread([this, sub, drain = std::move(drain)]() {
    namePollThread();
    while (_pollRunning.load(std::memory_order_acquire)) {
      drain(sub); // one kernel-park wait cycle; re-checks the run flag after
    }
  });
}

void HybridKevyNitro::subscribePush(
    const std::string& channel,
    const std::function<void(const std::shared_ptr<ArrayBuffer>&)>& onMessage) {
  spawnPoller(channel, [cb = onMessage](KevySub* sub) {
    KevyBuf out{};
    int32_t rc = kevy_sub_wait_raw(sub, kWaitSliceMs, &out); // kernel park
    if (rc == 1) {
      cb(takeBuf(out)); // wrap + GC-time free; zero-copy, like getData
    }
    // rc == 0: timeout or an ack was skipped; rc < 0: closed — either way the
    // outer loop re-checks the run flag.
  });
}

// Append one drained frame to the packed batch buffer as [u32-LE len][bytes],
// then free the KevyBuf. The JS side (unpackFrames) walks these prefixes to
// slice zero-copy Uint8Array views — so a batch of M frames crosses as ONE
// ArrayBuffer instead of M ArrayBuffer::move JSI allocations.
static void packFrame(std::vector<uint8_t>& packed, KevyBuf& buf) {
  uint32_t n = static_cast<uint32_t>(buf.len);
  packed.push_back(static_cast<uint8_t>(n & 0xFF));
  packed.push_back(static_cast<uint8_t>((n >> 8) & 0xFF));
  packed.push_back(static_cast<uint8_t>((n >> 16) & 0xFF));
  packed.push_back(static_cast<uint8_t>((n >> 24) & 0xFF));
  packed.insert(packed.end(), buf.ptr, buf.ptr + buf.len);
  kevy_buf_free(buf.ptr, buf.len, buf.cap);
}

void HybridKevyNitro::subscribePushBatched(
    const std::string& channel,
    const std::function<void(const std::shared_ptr<ArrayBuffer>&, double)>& onBatch) {
  spawnPoller(channel, [cb = onBatch](KevySub* sub) {
    KevyBuf out{};
    int32_t rc = kevy_sub_wait_raw(sub, kWaitSliceMs, &out); // block for frame 1
    if (rc != 1) {
      return; // timeout, ack skipped, or closed — outer loop re-checks the flag
    }
    // Pack the whole burst — frame 1 plus everything else already queued — into
    // one length-prefixed buffer, so it rides one JS hop as one AB. Raw
    // payloads only (kevy_sub_next_raw skips acks): never a control frame. This
    // deliberate copy (packFrame) coalesces M frames into one crossing.
    std::vector<uint8_t> packed;
    uint32_t count = 0;
    packFrame(packed, out);
    count++;
    KevyBuf more{};
    while (kevy_sub_next_raw(sub, &more) == 1) {
      packFrame(packed, more);
      count++;
    }
    cb(ArrayBuffer::move(std::move(packed)), static_cast<double>(count));
  });
}

void HybridKevyNitro::stopPush() { stopPushInternal(); }

void HybridKevyNitro::stopPushInternal() {
  _pollRunning.store(false, std::memory_order_release);
  if (_poller.joinable()) {
    _poller.join();
  }
  _pushSub.reset(); // closes via KevySubDeleter (no-op if already null)
}

} // namespace margelo::nitro::kevy
