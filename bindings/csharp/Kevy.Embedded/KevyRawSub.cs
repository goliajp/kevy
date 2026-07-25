// A low-level embedded subscription handle over one channel or pattern —
// the minimal kevy-embedded pub/sub surface (contract §5.2). PollRaw()
// drains one queued frame non-blocking; WaitRaw(ms) parks the thread up to
// the timeout. Each frame is the raw RESP array the server would push
// (*3 message …, *4 pmessage …, *3 subscribe/unsubscribe ack). The
// higher-level multi-channel Subscriber (§3.11) is built on top of these.

namespace Kevy.Embedded;

/// <summary>One embedded subscription handle; yields raw RESP frame bytes.</summary>
public sealed unsafe class KevyRawSub : IDisposable
{
    private IntPtr _handle;

    internal KevyRawSub(IntPtr handle) => _handle = handle;

    /// <summary>Drain one queued frame without blocking; null when empty.</summary>
    public byte[]? PollRaw()
    {
        if (_handle == IntPtr.Zero) throw new KevyException("kevy: closed subscription");
        KevyBuf @out;
        var rc = KevyNative.kevy_sub_next(_handle, &@out);
        return rc switch
        {
            1 => KevyNative.TakeBuf(in @out),
            0 => null,
            _ => throw new KevyException("kevy: subscription misuse"),
        };
    }

    /// <summary>Block up to <paramref name="timeoutMs"/> (0 = forever) for
    /// one frame; null on timeout.</summary>
    public byte[]? WaitRaw(ulong timeoutMs)
    {
        if (_handle == IntPtr.Zero) throw new KevyException("kevy: closed subscription");
        KevyBuf @out;
        var rc = KevyNative.kevy_sub_wait(_handle, timeoutMs, &@out);
        return rc switch
        {
            1 => KevyNative.TakeBuf(in @out),
            0 => null,
            _ => throw new KevyException("kevy: subscription closed"),
        };
    }

    /// <summary>Unsubscribe from everything this handle held.</summary>
    public void Dispose()
    {
        if (_handle != IntPtr.Zero)
        {
            KevyNative.kevy_sub_close(_handle);
            _handle = IntPtr.Zero;
        }
    }
}
