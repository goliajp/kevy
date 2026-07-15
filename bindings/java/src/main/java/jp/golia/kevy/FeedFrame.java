// FeedFrame — one applied effect in the change feed (client-contract §4.6).
// `offset` is monotonic within a generation; `argv` is the effect, e.g.
// ["SET","k","v"].
package jp.golia.kevy;

import java.util.List;

public record FeedFrame(long offset, List<byte[]> argv) {}
