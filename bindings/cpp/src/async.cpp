#include "kevy/async.hpp"

#include <memory>

#include "kevy/client.hpp"

namespace kevy {

// The async face over this same client (§1.4). Reached via Client::async();
// AsyncClient's methods delegate to the blocking Client methods on the
// Client's one reusable worker thread (created here on first use), so results
// agree byte-for-byte with the sync face.
AsyncClient Client::async() {
  if (!async_exec_) async_exec_ = std::make_unique<detail::AsyncExecutor>();
  return AsyncClient(this, async_exec_.get());
}

}  // namespace kevy
