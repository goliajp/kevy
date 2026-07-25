// kevy/enums.hpp — enums shared with the wire protocol and embedded store
// (contract §4.13 / §4.2 / §4.1).
#ifndef KEVY_ENUMS_HPP
#define KEVY_ENUMS_HPP

namespace kevy {

// AGGREGATE mode for ZINTERSTORE / ZUNIONSTORE. The default (Sum) emits no
// wire tag (§3.6).
enum class ZAggregate { Sum, Min, Max };

// Optional condition on the hash field-TTL verbs (§3.7). Always emits no wire
// keyword.
enum class HExpireCond { Always, Nx, Xx, Gt, Lt };

// Signed 8-bit per-field result code from the hash field-TTL verbs (§3.7).
using HExpireCode = signed char;

// Declared scalar type of a range index (§4.2) → wire tags i64/f64/str.
enum class IdxType { I64, F64, Str };

// Per-connection RESP dialect (§4.1).
enum class RespVersion { V2, V3 };

// Embedded store maxmemory behaviour (§5.3).
enum class EvictionPolicy {
  NoEviction,
  AllKeysLru,
  AllKeysLfu,
  AllKeysRandom,
  VolatileLru,
  VolatileLfu,
  VolatileRandom,
  VolatileTtl,
};

}  // namespace kevy

#endif  // KEVY_ENUMS_HPP
