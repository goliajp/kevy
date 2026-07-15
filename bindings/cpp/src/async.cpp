#include "kevy/async.hpp"

#include "kevy/client.hpp"

namespace kevy {

// The async face over this same client (§1.4). Reached via Client::async();
// AsyncClient's methods delegate to the blocking Client methods on a
// std::async thread, so results agree byte-for-byte with the sync face.
AsyncClient Client::async() { return AsyncClient(this); }

}  // namespace kevy
