#pragma once

#include "HybridKevyNitroSpec.hpp"
#include "kevy.h"

#include <atomic>
#include <functional>
#include <memory>
#include <thread>
#include <vector>

namespace margelo::nitro::kevy {

// RAII deleters for the two opaque kevy handles, so the db and subscriptions
// close exactly once at member-destruction time instead of by hand. Mirrors
// the buffer side, which is already RAII (ArrayBuffer::wrap's GC-time free).
struct KevyDbDeleter {
  void operator()(KevyDb* db) const noexcept {
    if (db != nullptr) {
      kevy_close(db);
    }
  }
};
struct KevySubDeleter {
  void operator()(KevySub* sub) const noexcept {
    if (sub != nullptr) {
      kevy_sub_close(sub);
    }
  }
};
using KevyDbPtr = std::unique_ptr<KevyDb, KevyDbDeleter>;
using KevySubPtr = std::unique_ptr<KevySub, KevySubDeleter>;

// The C++ side of the Nitro door. Inherits the Nitrogen-generated spec
// (which carries the JSI binding glue) and calls kevy-ffi directly. One
// in-memory db per instance, opened in the constructor; one optional raw
// subscription for the poll model, plus an optional push poller thread.
// Synchronous cmd() calls run on the JS thread — the MMKV shape, minus the
// Expo module dispatch the current door pays.
class HybridKevyNitro : public HybridKevyNitroSpec {
public:
  HybridKevyNitro() : HybridObject(TAG), _db(kevy_open_mem()) {}
  ~HybridKevyNitro() override {
    // Join the poller thread before the handles unwind (it holds _pushSub);
    // _pushSub / _sub / _db then close via their unique_ptr deleters.
    stopPushInternal();
  }

  // Registers getData/setData as RAW JSI methods (no typed-converter layer —
  // the converters' per-call NativeState attach + JSICache registration are
  // 40-90% of a small op's time; MMKV's hand-written HostObject shape). The
  // other nine methods register typed, exactly as the generated spec would.
  void loadHybridMethods() override;

  double abi() override;
  std::shared_ptr<ArrayBuffer> cmd(const std::shared_ptr<ArrayBuffer>& argv) override;

  // Scalar KV door — kevy_get / kevy_set directly, no argv, no RESP.
  std::optional<std::shared_ptr<ArrayBuffer>> getData(const std::string& key) override;
  void setData(const std::string& key,
               const std::shared_ptr<ArrayBuffer>& value, double ttlMs) override;
  // Re-open file-backed (durable) at dir, replacing the in-memory ctor db.
  bool openAt(const std::string& dir) override;

  void subscribe(const std::string& channel) override;
  double publish(const std::string& channel, const std::shared_ptr<ArrayBuffer>& payload) override;
  std::optional<std::shared_ptr<ArrayBuffer>> subNext() override;

  void subscribePush(
      const std::string& channel,
      const std::function<void(const std::shared_ptr<ArrayBuffer>&)>& onMessage) override;
  void subscribePushBatched(
      const std::string& channel,
      const std::function<void(const std::shared_ptr<ArrayBuffer>&, double)>& onBatch) override;
  void stopPush() override;

private:
  // Stop the poller thread (if any) and close the push subscription. Safe to
  // call from the JS thread: the poller never blocks on the JS thread (the
  // callback hop is fire-and-forget), so join() returns promptly.
  void stopPushInternal();

  // WRONGTYPE fallback for getData: the zero-copy shared lane collapses a
  // non-string key (its only error) into rc<0, so re-issue as a framed GET and
  // surface the -WRONGTYPE reply as a typed JS exception instead of a phantom
  // miss. Isomorphic to the C++ door's get_framed_fallback.
  std::optional<std::shared_ptr<ArrayBuffer>> getDataFramed(const std::string& key);

  // The raw JSI bodies behind loadHybridMethods' registerRawHybridMethod:
  // hand-rolled arg/return handling (jsi::JSError on the JS boundary, typed
  // std::exception → JSError like Nitro's typed path, so WRONGTYPE keeps
  // surfacing as the same typed error).
  facebook::jsi::Value getDataRaw(facebook::jsi::Runtime& runtime,
                                  const facebook::jsi::Value& thisValue,
                                  const facebook::jsi::Value* args, size_t count);
  facebook::jsi::Value setDataRaw(facebook::jsi::Runtime& runtime,
                                  const facebook::jsi::Value& thisValue,
                                  const facebook::jsi::Value* args, size_t count);
  facebook::jsi::Value publishRaw(facebook::jsi::Runtime& runtime,
                                  const facebook::jsi::Value& thisValue,
                                  const facebook::jsi::Value* args, size_t count);

  // Spawn the single push-poller thread: subscribe, mark the run flag, and
  // loop `drain(sub)` (one kernel-park wait cycle per call) until stopPush.
  // Both push lanes share this; only the per-cycle drain body differs.
  void spawnPoller(const std::string& channel, std::function<void(KevySub*)> drain);

  KevyDbPtr _db;
  KevySubPtr _sub;

  KevySubPtr _pushSub;
  std::thread _poller;
  std::atomic<bool> _pollRunning{false};
};

} // namespace margelo::nitro::kevy
