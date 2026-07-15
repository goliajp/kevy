// FeedBatch — one FEED.READ response (client-contract §4.7). Resume the next
// read from (generation, nextOffset). `frames` may be empty (caught up).
package jp.golia.kevy;

import java.util.List;

public record FeedBatch(long generation, long nextOffset, List<FeedFrame> frames) {}
