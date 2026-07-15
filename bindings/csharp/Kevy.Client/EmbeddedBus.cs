// The embedded side of the pub/sub consumer (contract §3.11 / §5.1). One
// low-level KevyRawSub handle per channel/pattern (the C ABI's pub/sub is
// per-channel); the store is shared through the process registry so a
// Publish on the same URL reaches here. Handles are polled round-robin and,
// on the single-handle common case, block efficiently via the FFI wait.

using System.Text;
using Kevy.Embedded;

namespace Kevy;

internal sealed class EmbeddedBus : IDisposable
{
    private KevyDb? _db;
    private readonly string _key;
    private readonly List<Handle> _handles = new();
    private int _rr;

    private sealed record Handle(KevyRawSub Sub, byte[] Name, bool Pattern);

    internal EmbeddedBus(KevyDb db, string key) { _db = db; _key = key; }

    internal void Add(byte[][] names, bool pattern)
    {
        var db = _db ?? throw new KevyClosedException();
        foreach (var n in names)
            _handles.Add(new Handle(db.OpenRawSub(n, pattern), (byte[])n.Clone(), pattern));
    }

    internal void Remove(byte[][] names, bool pattern)
    {
        var keep = new List<Handle>(_handles.Count);
        foreach (var h in _handles)
        {
            var drop = h.Pattern == pattern && (names.Length == 0 || Contains(names, h.Name));
            if (drop) h.Sub.Dispose();
            else keep.Add(h);
        }
        _handles.Clear();
        _handles.AddRange(keep);
    }

    // Recv blocks for the next raw frame across all handles. timeout null
    // waits forever; a bounded timeout that elapses throws TimedOut.
    internal byte[] Recv(TimeSpan? timeout)
    {
        var deadline = timeout is { } t ? DateTime.UtcNow.Add(t) : DateTime.MaxValue;
        while (true)
        {
            if (Poll() is { } frame) return frame;
            if (_handles.Count == 1)
            {
                var ms = timeout is null ? 0ul : (ulong)Math.Max(1, (deadline - DateTime.UtcNow).TotalMilliseconds);
                var got = _handles[0].Sub.WaitRaw(ms);
                if (got is not null) return got;
                if (timeout is not null) throw new KevyTimedOutException();
                continue;
            }
            if (timeout is not null && DateTime.UtcNow >= deadline) throw new KevyTimedOutException();
            Thread.Sleep(2);
        }
    }

    // Poll drains one queued frame from any handle, advancing the
    // round-robin cursor so no handle is starved.
    private byte[]? Poll()
    {
        var n = _handles.Count;
        for (var i = 0; i < n; i++)
        {
            var h = _handles[(_rr + i) % n];
            var got = h.Sub.PollRaw();
            if (got is not null) { _rr = (_rr + i + 1) % n; return got; }
        }
        return null;
    }

    private static bool Contains(byte[][] names, byte[] name)
    {
        foreach (var m in names) if (m.AsSpan().SequenceEqual(name)) return true;
        return false;
    }

    public void Dispose()
    {
        foreach (var h in _handles) h.Sub.Dispose();
        _handles.Clear();
        if (_db is not null) { Registry.Release(_key, _db); _db = null; }
    }
}
