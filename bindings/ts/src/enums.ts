// Enums shared with the embedded store and the wire protocol (contract
// §4.13 / §4.2). Modelled as const objects + string-literal union types
// rather than TS `enum`, so the source stays erasable (runs on Node's
// type-stripping and Bun with no build step) while keeping strong types.

/** AGGREGATE mode for ZINTERSTORE / ZUNIONSTORE (§3.6). Sum is the default
 * and emits no wire tag. */
export type ZAggregate = "sum" | "min" | "max";

export function aggregateTag(a: ZAggregate): string {
  return a === "min" ? "MIN" : a === "max" ? "MAX" : "SUM";
}

/** Optional condition on the hash field-TTL verbs (§3.7). "always" emits no
 * wire keyword. */
export type HExpireCond = "always" | "nx" | "xx" | "gt" | "lt";

export function condKeyword(c: HExpireCond): string | null {
  switch (c) {
    case "nx":
      return "NX";
    case "xx":
      return "XX";
    case "gt":
      return "GT";
    case "lt":
      return "LT";
    default:
      return null;
  }
}

/** The declared scalar type of a range index (§4.2). */
export type IdxType = "i64" | "f64" | "str";

/** The per-connection RESP dialect (§4.1). */
export type RespVersion = "v2" | "v3";

/** The embedded store's maxmemory behaviour (§5.3). */
export type EvictionPolicy =
  | "noEviction"
  | "allKeysLru"
  | "allKeysLfu"
  | "allKeysRandom"
  | "volatileLru"
  | "volatileLfu"
  | "volatileRandom"
  | "volatileTtl";
