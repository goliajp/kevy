// EvictionPolicy — maxmemory eviction policy for the embedded store
// (client-contract §5.3, §4.13).
package jp.golia.kevy;

public enum EvictionPolicy {
    NO_EVICTION, ALL_KEYS_LRU, ALL_KEYS_LFU, ALL_KEYS_RANDOM,
    VOLATILE_LRU, VOLATILE_LFU, VOLATILE_RANDOM, VOLATILE_TTL
}
