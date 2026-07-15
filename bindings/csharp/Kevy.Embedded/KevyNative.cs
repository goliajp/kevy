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

    [LibraryImport(Lib)]
    internal static partial void kevy_close(IntPtr db);

    [LibraryImport(Lib)]
    internal static partial int kevy_cmd(
        IntPtr db, nuint argc, byte** argv, nuint* argvLen, KevyBuf* @out);

    [LibraryImport(Lib)]
    internal static partial void kevy_buf_free(byte* ptr, nuint len, nuint cap);

    [LibraryImport(Lib)]
    internal static partial int kevy_get(IntPtr db, byte* key, nuint keyLen, KevyBuf* @out);

    [LibraryImport(Lib)]
    internal static partial int kevy_set(
        IntPtr db, byte* key, nuint keyLen, byte* val, nuint valLen, ulong ttlMs);

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

    /// <summary>Copy a reply buffer into managed bytes, then free it.</summary>
    internal static byte[] TakeBuf(in KevyBuf buf)
    {
        if (buf.Len == 0) return [];
        var bytes = new ReadOnlySpan<byte>(buf.Ptr, checked((int)buf.Len)).ToArray();
        kevy_buf_free(buf.Ptr, buf.Len, buf.Cap);
        return bytes;
    }
}
