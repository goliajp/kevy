// FeedTail — where a fresh/resumed feed consumer begins (client-contract
// §3.10): the current generation plus the next offset to read from.
package jp.golia.kevy;

public record FeedTail(long generation, long nextOffset) {}
