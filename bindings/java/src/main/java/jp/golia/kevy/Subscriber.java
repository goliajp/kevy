// Subscriber — the pub/sub consumer side (client-contract §3.11). A subscribed
// connection cannot send normal commands, so this is a distinct type;
// publishing is done from a normal KevyClient via publish(). Backends by URL:
// kevy:// / redis:// / tcp:// use a dedicated TCP socket; mem://<name> /
// file:// use the in-process bus. Anonymous mem:// is rejected (no other
// producer — use a named bus). recv() accepts both RESP2 arrays and RESP3
// push frames transparently.
package jp.golia.kevy;

import java.nio.charset.StandardCharsets;
import java.time.Duration;
import java.util.ArrayList;
import java.util.Iterator;
import java.util.List;
import java.util.NoSuchElementException;

public final class Subscriber implements AutoCloseable {
    private RespConn remote;
    private EmbSub emb;
    private Duration readTimeout;

    private Subscriber(RespConn remote, EmbSub emb) {
        this.remote = remote;
        this.emb = emb;
    }

    /** Open a subscriber without subscribing yet. Anonymous mem:// is rejected. */
    public static Subscriber connect(String url) {
        Url.Target t = Url.parse(url);
        if (t instanceof Url.MemAnon) {
            throw new UnsupportedException("anonymous mem:// has no other producer; use mem://<name> for a shared bus");
        }
        if (t instanceof Url.Remote r) {
            RespConn conn = RespConn.dial(r.host(), r.port());
            return new Subscriber(conn, null);
        }
        return new Subscriber(null, new EmbSub(Registry.acquire(t)));
    }

    /** Open and subscribe in one step (≥1 channel). */
    public static Subscriber connectChannels(String url, byte[]... channels) {
        if (channels.length == 0) throw new InvalidInputException("connectChannels needs ≥ 1 channel");
        Subscriber s = connect(url);
        try {
            s.subscribe(channels);
        } catch (RuntimeException e) {
            s.close();
            throw e;
        }
        return s;
    }

    public void subscribe(byte[]... channels) {
        if (channels.length == 0) throw new InvalidInputException("SUBSCRIBE needs ≥ 1 channel");
        if (remote != null) remote.writeOnly(Argv.cmd("SUBSCRIBE").addAll(channels).list());
        else emb.add(channels, false);
    }

    public void psubscribe(byte[]... patterns) {
        if (patterns.length == 0) throw new InvalidInputException("PSUBSCRIBE needs ≥ 1 pattern");
        if (remote != null) remote.writeOnly(Argv.cmd("PSUBSCRIBE").addAll(patterns).list());
        else emb.add(patterns, true);
    }

    /** Unsubscribe from channels (empty = all). */
    public void unsubscribe(byte[]... channels) {
        if (remote != null) remote.writeOnly(Argv.cmd("UNSUBSCRIBE").addAll(channels).list());
        else emb.remove(channels, false);
    }

    public void punsubscribe(byte[]... patterns) {
        if (remote != null) remote.writeOnly(Argv.cmd("PUNSUBSCRIBE").addAll(patterns).list());
        else emb.remove(patterns, true);
    }

    /** Negotiate RESP3 push frames; remote-only, must precede any subscribe. */
    public PubsubEvent hello3() {
        if (remote == null) throw new UnsupportedException("HELLO 3 is remote-only; the embedded bus has no proto switch");
        Reply r = remote.request(Argv.cmd("HELLO").add("3").list());
        if (r.isError()) throw Errors.fromReplyText(r.payload());
        if (r instanceof Reply.Map || r.items() != null) {
            return new PubsubEvent(PubsubEvent.Kind.SUBSCRIBE, Bytes.of("HELLO"), null, null, 3);
        }
        throw Errors.unexpected(r, "HELLO 3 map/array reply");
    }

    /** Block for the next frame (acks + deliveries). Close → Closed, timeout → TimedOut. */
    public PubsubEvent recv() {
        Reply r = remote != null ? remote.readOne() : emb.recv(readTimeout);
        return classify(r);
    }

    /** Block for the next Message/Pmessage, skipping ack frames. */
    public PubsubEvent recvMessage() {
        while (true) {
            PubsubEvent ev = recv();
            if (ev.isMessage()) return ev;
        }
    }

    /** Bound a blocking recv (null clears it). A timeout surfaces as TimedOut. */
    public void setReadTimeout(Duration dur) {
        this.readTimeout = dur;
        if (remote != null) remote.setReadTimeout(dur);
    }

    /** A stream of events, terminating on Closed. */
    public Iterator<PubsubEvent> events() {
        return iterate(false);
    }

    /** A stream of messages (ack frames silently skipped), terminating on Closed. */
    public Iterator<PubsubEvent> messages() {
        return iterate(true);
    }

    private Iterator<PubsubEvent> iterate(boolean messagesOnly) {
        return new Iterator<>() {
            private PubsubEvent nextEv;
            private boolean done;

            @Override
            public boolean hasNext() {
                if (done) return false;
                if (nextEv != null) return true;
                try {
                    nextEv = messagesOnly ? recvMessage() : recv();
                    return true;
                } catch (ClosedException e) {
                    done = true;
                    return false;
                }
            }

            @Override
            public PubsubEvent next() {
                if (!hasNext()) throw new NoSuchElementException();
                PubsubEvent ev = nextEv;
                nextEv = null;
                return ev;
            }
        };
    }

    @Override
    public void close() {
        if (remote != null) {
            remote.close();
            remote = null;
        }
        if (emb != null) {
            emb.close();
            emb = null;
        }
    }

    // ---- frame classification (RESP2 array or RESP3 push) ----
    private static PubsubEvent classify(Reply r) {
        if (r.isError()) throw Errors.fromReplyText(r.payload());
        List<Reply> items = r.items();
        if (items == null) throw new ProtocolException("expected array/push frame, got " + r.shape());
        if (items.isEmpty() || items.get(0).payload() == null) {
            throw new ProtocolException("pubsub frame missing kind field");
        }
        String kind = new String(items.get(0).payload(), StandardCharsets.UTF_8);
        return shape(kind, items);
    }

    private static PubsubEvent shape(String kind, List<Reply> items) {
        return switch (kind) {
            case "subscribe" -> ack(PubsubEvent.Kind.SUBSCRIBE, items, false);
            case "psubscribe" -> ack(PubsubEvent.Kind.PSUBSCRIBE, items, true);
            case "unsubscribe" -> ack(PubsubEvent.Kind.UNSUBSCRIBE, items, false);
            case "punsubscribe" -> ack(PubsubEvent.Kind.PUNSUBSCRIBE, items, true);
            case "message" -> {
                if (items.size() != 3) throw new ProtocolException("bad message frame");
                yield new PubsubEvent(PubsubEvent.Kind.MESSAGE, items.get(1).payload(), null, items.get(2).payload(), 0);
            }
            case "pmessage" -> {
                if (items.size() != 4) throw new ProtocolException("bad pmessage frame");
                yield new PubsubEvent(PubsubEvent.Kind.PMESSAGE, items.get(2).payload(), items.get(1).payload(), items.get(3).payload(), 0);
            }
            default -> throw new ProtocolException("unknown pubsub kind '" + kind + "'");
        };
    }

    private static PubsubEvent ack(PubsubEvent.Kind kind, List<Reply> items, boolean pattern) {
        if (items.size() != 3) throw new ProtocolException("expected 3-element pubsub frame");
        byte[] name = items.get(1).isNil() ? null : items.get(1).payload();
        if (!(items.get(2) instanceof Reply.Int cnt)) throw new ProtocolException("bad pubsub count field");
        byte[] channel = pattern ? null : name;
        byte[] pat = pattern ? name : null;
        return new PubsubEvent(kind, channel, pat, null, cnt.value());
    }
}
