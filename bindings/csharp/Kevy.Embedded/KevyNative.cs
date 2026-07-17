// KevyNative — the raw P/Invoke surface over libkevy_ffi (crates/kevy-ffi),
// the same discipline as the Go and JNI doors: handles are opaque, every
// payload is bytes, and the typed ergonomics live one floor up (KevyDb).
//
// LibraryImport (compile-time marshalling, net8) over DllImport: every
// signature here is blittable, so nothing needs a runtime marshaller.

using System.Reflection;
using System.Runtime.InteropServices;

namespace Kevy.Embedded;

/// <summary>A byte buffer owned by the engine — consume via
/// <see cref="KevyNative.TakeBuf"/>, which copies out and frees.</summary>
[StructLayout(LayoutKind.Sequential)]
internal unsafe struct KevyBuf
{
    public byte* Ptr;
    public nuint Len;
    public nuint Cap;
}

/// <summary>The boot-replay verdict kevy_open_report fills — field-for-field
/// the C ABI's KevyOpenReport (four u64s, a u8 flag, a padded u32 count).</summary>
[StructLayout(LayoutKind.Sequential)]
internal struct KevyOpenReport
{
    public ulong ReplayedCommands;
    public ulong ReplayedBytes;
    public ulong ElapsedMs;
    public ulong DroppedBytes;
    public byte Corrupt;
    public uint QuarantineCount;
}

/// <summary>The open policy kevy_open_with reads — field-for-field the C
/// ABI's KevyOpenOptions (a u8, two u32s, three u64s; sequential natural
/// alignment puts them at offsets 0, 4, 8, 16, 24, 32 — size 40, exactly
/// the C layout).</summary>
[StructLayout(LayoutKind.Sequential)]
internal struct KevyOpenOptionsNative
{
    public byte Fsync;
    public uint Shards;
    public uint RewritePct;
    public ulong RewriteMinSize;
    public ulong RewriteBytes;
    public ulong RewriteIntervalSecs;
}

internal static unsafe partial class KevyNative
{
    private const string Lib = "kevy_ffi";

    static KevyNative()
    {
        NativeLibrary.SetDllImportResolver(typeof(KevyNative).Assembly, Resolve);
    }

    // KEVY_FFI_LIB points straight at the cdylib (development / ffigate);
    // without it the default probe order runs, which finds the NuGet
    // runtimes/&lt;rid&gt;/native layout in a published app.
    private static IntPtr Resolve(string name, Assembly assembly, DllImportSearchPath? searchPath)
    {
        if (name != Lib) return IntPtr.Zero;
        var env = Environment.GetEnvironmentVariable("KEVY_FFI_LIB");
        return env is null ? IntPtr.Zero : NativeLibrary.Load(env);
    }

    [LibraryImport(Lib)]
    internal static partial uint kevy_abi();

    [LibraryImport(Lib)]
    internal static partial byte* kevy_version();

    [LibraryImport(Lib)]
    internal static partial IntPtr kevy_open(byte* dir, nuint dirLen);

    [LibraryImport(Lib)]
    internal static partial IntPtr kevy_open_mem();

    // kevy_open with explicit options: durable when dir is non-null,
    // in-memory when dir is null + dirLen 0; null opts = kevy_open's
    // defaults. Null on failure.
    [LibraryImport(Lib)]
    internal static partial IntPtr kevy_open_with(
        byte* dir, nuint dirLen, KevyOpenOptionsNative* opts);

    [LibraryImport(Lib)]
    internal static partial void kevy_close(IntPtr db);

    // Flush every shard's AOF (a REAL fsync) + feed marker, then refuse
    // later writes (reads stay). Idempotent. 0 ok, -1 misuse, -2 I/O.
    [LibraryImport(Lib)]
    internal static partial int kevy_shutdown(IntPtr db);

    [LibraryImport(Lib)]
    internal static partial int kevy_cmd(
        IntPtr db, nuint argc, byte** argv, nuint* argvLen, KevyBuf* @out);

    [LibraryImport(Lib)]
    internal static partial void kevy_buf_free(byte* ptr, nuint len, nuint cap);

    [LibraryImport(Lib)]
    internal static partial int kevy_get(IntPtr db, byte* key, nuint keyLen, KevyBuf* @out);

    // Zero-copy shared GET lane. On a hit the returned KevyBuf's `Cap` is an
    // OPAQUE tagged owner handle (an engine Arc pointer or a tagged Vec
    // capacity), NOT a plain allocation capacity. It MUST be released with
    // kevy_buf_free_shared — passing it to kevy_buf_free misreclaims the
    // engine's Arc/Vec (memory corruption). Pairs 1:1 with the shared free.
    [LibraryImport(Lib)]
    internal static partial int kevy_get_shared(IntPtr db, byte* key, nuint keyLen, KevyBuf* @out);

    // Frees a buffer from kevy_get_shared, dropping the engine Arc/Vec. Never
    // call this on a plain kevy_get / kevy_cmd buffer, and never free a shared
    // buffer with kevy_buf_free.
    [LibraryImport(Lib)]
    internal static partial void kevy_buf_free_shared(byte* ptr, nuint len, nuint cap);

    [LibraryImport(Lib)]
    internal static partial int kevy_set(
        IntPtr db, byte* key, nuint keyLen, byte* val, nuint valLen, ulong ttlMs);

    [LibraryImport(Lib)]
    internal static partial int kevy_open_report(IntPtr db, KevyOpenReport* @out);

    [LibraryImport(Lib)]
    internal static partial IntPtr kevy_subscribe(IntPtr db, byte* chan, nuint chanLen);

    [LibraryImport(Lib)]
    internal static partial IntPtr kevy_psubscribe(IntPtr db, byte* pat, nuint patLen);

    [LibraryImport(Lib)]
    internal static partial int kevy_sub_next(IntPtr sub, KevyBuf* @out);

    // Block up to timeout_ms (0 = forever) for one frame: 1 frame, 0
    // timeout, negative misuse/bus-gone. Parks the thread — no busy-poll.
    [LibraryImport(Lib)]
    internal static partial int kevy_sub_wait(IntPtr sub, ulong timeoutMs, KevyBuf* @out);

    [LibraryImport(Lib)]
    internal static partial void kevy_sub_close(IntPtr sub);

    /// <summary>Copy a plain-lane reply buffer into managed bytes, then free
    /// it (kevy_cmd / kevy_get). `Cap` is a real allocation capacity here.</summary>
    internal static byte[] TakeBuf(in KevyBuf buf)
    {
        if (buf.Len == 0) return [];
        var bytes = new ReadOnlySpan<byte>(buf.Ptr, checked((int)buf.Len)).ToArray();
        kevy_buf_free(buf.Ptr, buf.Len, buf.Cap);
        return bytes;
    }

    /// <summary>Copy a shared-lane GET buffer into managed bytes, then release
    /// the engine Arc/Vec via kevy_buf_free_shared. Use ONLY for buffers from
    /// <see cref="kevy_get_shared"/>: the tagged <c>Cap</c> owner handle is not
    /// a plain capacity, so <see cref="kevy_buf_free"/> would misreclaim it. A
    /// shared hit is always freed here, even when empty (its Cap owns memory).</summary>
    internal static byte[] TakeBufShared(in KevyBuf buf)
    {
        var bytes = buf.Len == 0
            ? []
            : new ReadOnlySpan<byte>(buf.Ptr, checked((int)buf.Len)).ToArray();
        kevy_buf_free_shared(buf.Ptr, buf.Len, buf.Cap);
        return bytes;
    }
}
