// PubsubEvent — a decoded pub/sub frame (client-contract §4.8). A flat record
// with a Kind discriminant (non-exhaustive: tolerate future kinds). `channel`
// / `pattern` / `payload` are populated per kind; `count` = total channels +
// patterns held after the op (for the ack kinds). Unsubscribe/Punsubscribe
// carry a null channel/pattern to mean "all".
package jp.golia.kevy;

public record PubsubEvent(Kind kind, byte[] channel, byte[] pattern, byte[] payload, long count) {

    public enum Kind {
        SUBSCRIBE, PSUBSCRIBE, UNSUBSCRIBE, PUNSUBSCRIBE, MESSAGE, PMESSAGE
    }

    /** True for a MESSAGE/PMESSAGE delivery (vs a subscription ack). */
    public boolean isMessage() {
        return kind == Kind.MESSAGE || kind == Kind.PMESSAGE;
    }

    public String channelStr() { return Bytes.str(channel); }
    public String patternStr() { return Bytes.str(pattern); }
    public String payloadStr() { return Bytes.str(payload); }
}
