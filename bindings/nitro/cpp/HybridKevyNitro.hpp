#pragma once

#include "HybridKevyNitroSpec.hpp"
#include "kevy.h"

#include <atomic>
#include <functional>
#include <memory>
#include <thread>
#include <vector>

namespace margelo::nitro::kevy {

// The C++ side of the Nitro door. Inherits the Nitrogen-generated spec
// (which carries the JSI binding glue) and calls kevy-ffi directly. One
// in-memory db per instance, opened in the constructor; one optional raw
// subscription for the poll model, plus an optional push poller thread.
// Synchronous cmd() calls run on the JS thread — the MMKV shape, minus the
// Expo module dispatch the current door pays.
class HybridKevyNitro : public HybridKevyNitroSpec {
public:
  HybridKevyNitro() : HybridObject(TAG) {
    _db = kevy_open_mem();
  }
  ~HybridKevyNitro() override {
    stopPushInternal();
    if (_sub != nullptr) {
      kevy_sub_close(_sub);
    }
    if (_db != nullptr) {
      kevy_close(_db);
    }
  }

  double abi() override;
  std::shared_ptr<ArrayBuffer> cmd(const std::shared_ptr<ArrayBuffer>& argv) override;

  // Scalar KV door — kevy_get / kevy_set directly, no argv, no RESP.
  std::optional<std::shared_ptr<ArrayBuffer>> getData(const std::shared_ptr<ArrayBuffer>& key) override;
  void setData(const std::shared_ptr<ArrayBuffer>& key,
               const std::shared_ptr<ArrayBuffer>& value, double ttlMs) override;
  // Re-open file-backed (durable) at dir, replacing the in-memory ctor db.
  bool openAt(const std::string& dir) override;

  void subscribe(const std::string& channel) override;
  void publish(const std::string& channel, const std::shared_ptr<ArrayBuffer>& payload) override;
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

  KevyDb* _db = nullptr;
  KevySub* _sub = nullptr;

  KevySub* _pushSub = nullptr;
  std::thread _poller;
  std::atomic<bool> _pollRunning{false};
};

} // namespace margelo::nitro::kevy
