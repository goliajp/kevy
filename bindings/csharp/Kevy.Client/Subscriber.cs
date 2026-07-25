// Pub/sub consumer side (contract §3.11). A subscribed connection cannot
// send normal commands, so the consumer is a distinct type; publishing is
// done from a normal KevyClient via Publish. Backends by URL: kevy:// /
// redis:// / tcp:// use a dedicated TCP socket; mem://<name> / file:// use
// the in-process bus. Anonymous mem:// is rejected (no other producer — use
// a named bus). recv accepts both RESP2 array and RESP3 push shapes. Both
// faces: Recv/RecvAsync, RecvMessage/RecvMessageAsync.

namespace Kevy;

/// <summary>One subscribed connection — the pub/sub consumer (§3.11).</summary>
public sealed class Subscriber : IDisposable
{
    private RespConnection? _remote;
    private EmbeddedBus? _emb;
    private TimeSpan? _readTimeout;

    private Subscriber() { }

    /// <summary>Open a subscriber without subscribing yet. Anonymous mem://
    /// is rejected.</summary>
    public static Subscriber Connect(string url)
    {
        var t = Url.Parse(url);
        var s = new Subscriber();
        switch (t.Kind)
        {
            case TargetKind.MemAnon:
                throw new KevyUnsupportedException(
                    "anonymous mem:// has no other producer; use mem://<name> for a shared bus");
            case TargetKind.MemNamed or TargetKind.File:
                var (db, key) = Registry.Resolve(t);
                s._emb = new EmbeddedBus(db, key);
                break;
            default:
                s._remote = RespConnection.Dial(t.Host, t.Port);
                break;
        }
        return s;
    }

    /// <summary>Connect and subscribe in one step (≥1 channel required).</summary>
    public static Subscriber ConnectChannels(string url, params KevyBytes[] channels)
    {
        if (channels.Length == 0)
            throw new KevyInvalidInputException(
                "ConnectChannels needs ≥ 1 channel — use Connect for an empty start");
        var s = Connect(url);
        try { s.Subscribe(channels); }
        catch { s.Dispose(); throw; }
        return s;
    }

    /// <summary>SUBSCRIBE to channels (≥1).</summary>
    public void Subscribe(params KevyBytes[] channels)
    {
        if (channels.Length == 0) throw new KevyInvalidInputException("SUBSCRIBE needs ≥ 1 channel");
        if (_remote is not null) _remote.Write(Argv.Cmd("SUBSCRIBE", channels.Raw()));
        else _emb!.Add(channels.Raw(), false);
    }

    /// <summary>PSUBSCRIBE to glob patterns (≥1).</summary>
    public void Psubscribe(params KevyBytes[] patterns)
    {
        if (patterns.Length == 0) throw new KevyInvalidInputException("PSUBSCRIBE needs ≥ 1 pattern");
        if (_remote is not null) _remote.Write(Argv.Cmd("PSUBSCRIBE", patterns.Raw()));
        else _emb!.Add(patterns.Raw(), true);
    }

    /// <summary>UNSUBSCRIBE from channels (empty = all).</summary>
    public void Unsubscribe(params KevyBytes[] channels)
    {
        if (_remote is not null) _remote.Write(Argv.Cmd("UNSUBSCRIBE", channels.Raw()));
        else _emb!.Remove(channels.Raw(), false);
    }

    /// <summary>PUNSUBSCRIBE from patterns (empty = all).</summary>
    public void Punsubscribe(params KevyBytes[] patterns)
    {
        if (_remote is not null) _remote.Write(Argv.Cmd("PUNSUBSCRIBE", patterns.Raw()));
        else _emb!.Remove(patterns.Raw(), true);
    }

    /// <summary>Negotiate RESP3 push frames; remote-only, and must precede
    /// any subscribe (§3.11).</summary>
    public PubsubEvent Hello3()
    {
        var conn = _remote ?? throw new KevyUnsupportedException(
            "HELLO 3 is remote/TCP-only; the embedded bus has no proto switch");
        conn.Write(["HELLO"u8.ToArray(), "3"u8.ToArray()]);
        var r = conn.ReadReply();
        return r.Kind switch
        {
            ReplyKind.Map or ReplyKind.Array => new PubsubEvent(PubsubKind.Subscribe, "HELLO"u8.ToArray(), null, null, 3),
            ReplyKind.Error => throw Err.ReplyError(r),
            _ => throw new KevyProtocolException("unexpected HELLO 3 reply shape: " + r.Shape()),
        };
    }

    /// <summary>Block for the next frame (acks and deliveries). Close →
    /// Closed; read-timeout → TimedOut.</summary>
    public PubsubEvent Recv() =>
        PubsubFrame.Classify(_remote is not null ? _remote.ReadReply() : Reply.Decode(_emb!.Recv(_readTimeout)));

    /// <summary>Async <see cref="Recv"/>.</summary>
    public async Task<PubsubEvent> RecvAsync(CancellationToken ct = default)
    {
        if (_remote is not null) return PubsubFrame.Classify(await _remote.ReadReplyAsync(ct).ConfigureAwait(false));
        return await Task.Run(() => PubsubFrame.Classify(Reply.Decode(_emb!.Recv(_readTimeout))), ct).ConfigureAwait(false);
    }

    /// <summary>Block for the next Message/Pmessage, skipping ack frames.</summary>
    public (byte[] Channel, byte[] Payload) RecvMessage()
    {
        while (true)
        {
            var ev = Recv();
            if (ev.Kind is PubsubKind.Message or PubsubKind.Pmessage) return (ev.Channel!, ev.Payload!);
        }
    }

    /// <summary>Async <see cref="RecvMessage"/>.</summary>
    public async Task<(byte[] Channel, byte[] Payload)> RecvMessageAsync(CancellationToken ct = default)
    {
        while (true)
        {
            var ev = await RecvAsync(ct).ConfigureAwait(false);
            if (ev.Kind is PubsubKind.Message or PubsubKind.Pmessage) return (ev.Channel!, ev.Payload!);
        }
    }

    /// <summary>Bound a blocking Recv (null clears it). A timeout surfaces
    /// as TimedOut.</summary>
    public void SetReadTimeout(TimeSpan? d)
    {
        _readTimeout = d;
        _remote?.SetReadTimeout(d);
    }

    /// <summary>Stream frames until the connection closes (acks + deliveries).</summary>
    public IEnumerable<PubsubEvent> Events()
    {
        while (true)
        {
            PubsubEvent ev;
            try { ev = Recv(); }
            catch (KevyClosedException) { yield break; }
            yield return ev;
        }
    }

    /// <summary>Stream Message/Pmessage payloads (ack frames skipped).</summary>
    public IEnumerable<(byte[] Channel, byte[] Payload)> Messages()
    {
        foreach (var ev in Events())
            if (ev.Kind is PubsubKind.Message or PubsubKind.Pmessage)
                yield return (ev.Channel!, ev.Payload!);
    }

    /// <summary>Tear down the subscriber.</summary>
    public void Dispose()
    {
        if (_remote is not null) { _remote.Dispose(); _remote = null; }
        if (_emb is not null) { _emb.Dispose(); _emb = null; }
    }
}
