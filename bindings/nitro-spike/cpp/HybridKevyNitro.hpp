#pragma once

#include "HybridKevyNitroSpec.hpp"
#include "kevy.h"

namespace margelo::nitro::kevy {

// The C++ side of the Nitro door. Inherits the Nitrogen-generated spec
// (which carries the JSI binding glue) and calls kevy-ffi directly. One
// in-memory db per instance, opened in the constructor; one optional raw
// subscription. Synchronous, JS-thread — the MMKV shape, minus the Expo
// module dispatch the current door pays.
class HybridKevyNitro : public HybridKevyNitroSpec {
public:
  HybridKevyNitro() : HybridObject(TAG) {
    _db = kevy_open_mem();
  }
  ~HybridKevyNitro() override {
    if (_sub != nullptr) {
      kevy_sub_close(_sub);
    }
    if (_db != nullptr) {
      kevy_close(_db);
    }
  }

  double abi() override;
  std::shared_ptr<ArrayBuffer> cmd(const std::shared_ptr<ArrayBuffer>& argv) override;
  void subscribe(const std::string& channel) override;
  void publish(const std::string& channel, const std::shared_ptr<ArrayBuffer>& payload) override;
  std::optional<std::shared_ptr<ArrayBuffer>> subNext() override;

private:
  KevyDb* _db = nullptr;
  KevySub* _sub = nullptr;
};

} // namespace margelo::nitro::kevy
