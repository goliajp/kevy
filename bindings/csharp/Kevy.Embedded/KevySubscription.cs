// A polled subscription: Poll() drains pending frames into the callback,
// Dispose() unsubscribes. Owned by the KevyDb that opened it.

namespace Kevy.Embedded;

public sealed class KevySubscription : IDisposable
{
    private IntPtr _handle;
    private readonly Action<byte[], string> _callback;

    internal KevySubscription(IntPtr handle, Action<byte[], string> callback)
    {
        _handle = handle;
        _callback = callback;
    }

    public bool IsClosed => _handle == IntPtr.Zero;

    /// <summary>Drain pending frames, invoking the callback for each
    /// message / pmessage (subscribe acks are consumed silently). Returns
    /// the number of frames drained.</summary>
    public unsafe int Poll()
    {
        var n = 0;
        while (_handle != IntPtr.Zero)
        {
            KevyBuf @out;
            var rc = KevyNative.kevy_sub_next(_handle, &@out);
            if (rc != 1) break;
            n += 1;
            Dispatch(KevyValue.Parse(KevyNative.TakeBuf(in @out)));
        }
        return n;
    }

    private void Dispatch(KevyValue frame)
    {
        if (frame is not KevyValue.Array a) return;
        var items = a.Items;
        switch (items.Count > 0 ? items[0].AsText : null)
        {
            // *3: message <channel> <payload>
            case "message" when items.Count == 3:
            {
                if (items[1].AsText is { } chan && items[2] is KevyValue.Bulk p)
                    _callback(p.Bytes, chan);
                break;
            }
            // *4: pmessage <pattern> <channel> <payload>
            case "pmessage" when items.Count == 4:
            {
                if (items[2].AsText is { } chan && items[3] is KevyValue.Bulk p)
                    _callback(p.Bytes, chan);
                break;
            }
        }
    }

    public void Dispose()
    {
        if (_handle != IntPtr.Zero)
        {
            KevyNative.kevy_sub_close(_handle);
            _handle = IntPtr.Zero;
        }
    }
}
