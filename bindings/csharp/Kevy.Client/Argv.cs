// Argument plumbing. The wire is bytes (contract §7): the client never
// forces UTF-8 on user data. Rather than duplicate every method into a
// string overload and a byte[] overload, a single KevyBytes carrier
// implicitly accepts BOTH — `client.Set("k","v")` and
// `client.Set(keyBytes, valBytes)` both compile, and only the byte[] ever
// reaches the wire. Argv holds the small encoders (verb / int / float →
// bytes) shared across the command families.

using System.Buffers;
using System.Buffers.Text;
using System.Text;

namespace Kevy;

/// <summary>A binary-safe argument that accepts a UTF-8 <see cref="string"/>
/// or a raw <see cref="byte"/>[] interchangeably (contract §7). The wire
/// only ever sees bytes.</summary>
public readonly struct KevyBytes
{
    /// <summary>The raw bytes.</summary>
    public byte[] Raw { get; }

    private KevyBytes(byte[] raw) => Raw = raw;

    /// <summary>Wrap raw bytes.</summary>
    public static implicit operator KevyBytes(byte[] b) => new(b);

    /// <summary>Encode a UTF-8 string.</summary>
    public static implicit operator KevyBytes(string s) => new(Encoding.UTF8.GetBytes(s));
}

internal static class Argv
{
    internal static byte[] S(string s) => Encoding.ASCII.GetBytes(s);

    // Integer / float args format straight into a stack buffer (Utf8Formatter
    // is culture-invariant) — no interim string, one byte[] copy for the wire.
    internal static byte[] I(long n)
    {
        Span<byte> buf = stackalloc byte[20]; // long.MinValue = 20 chars incl. sign
        Utf8Formatter.TryFormat(n, buf, out var written);
        return buf[..written].ToArray();
    }

    internal static byte[] U(ulong n)
    {
        Span<byte> buf = stackalloc byte[20]; // ulong.MaxValue = 20 digits
        Utf8Formatter.TryFormat(n, buf, out var written);
        return buf[..written].ToArray();
    }

    // Shortest round-trippable form ('R'), matching the Rust/Go 'g' formatting.
    internal static byte[] F(double f)
    {
        Span<byte> buf = stackalloc byte[32];
        Utf8Formatter.TryFormat(f, buf, out var written, new StandardFormat('R'));
        return buf[..written].ToArray();
    }

    internal static byte[][] Raw(this KevyBytes[] xs)
    {
        var outp = new byte[xs.Length][];
        for (var i = 0; i < xs.Length; i++) outp[i] = xs[i].Raw;
        return outp;
    }

    // Prepend a verb to raw args → a fresh argv.
    internal static byte[][] Cmd(string verb, byte[][] rest)
    {
        var outp = new byte[rest.Length + 1][];
        outp[0] = S(verb);
        Array.Copy(rest, 0, outp, 1, rest.Length);
        return outp;
    }

    // Verb + key + rest (the collection-command shape).
    internal static byte[][] Cmd2(string verb, byte[] key, byte[][] rest)
    {
        var outp = new byte[rest.Length + 2][];
        outp[0] = S(verb);
        outp[1] = key;
        Array.Copy(rest, 0, outp, 2, rest.Length);
        return outp;
    }

    internal static long DurationMs(TimeSpan ttl)
    {
        var ms = (long)ttl.TotalMilliseconds;
        return ms < 0 ? 0 : ms;
    }
}
