// RESP decode for kevy replies — the C# twin of the Go / Kotlin / JS
// parsers. One complete reply in, one KevyValue out:
//   +OK      -> Simple("OK")
//   -ERR …   -> Error (returned, not thrown — the engine saying no is
//               data; throwing is reserved for ABI misuse)
//   :42      -> Int(42)
//   $N …     -> Bulk(bytes)
//   $-1/*-1  -> Nil
//   *N …     -> Array(items)

using System.Text;

namespace Kevy.Embedded;

/// <summary>Thrown on ABI misuse and by the typed surface on a protocol
/// error. <see cref="KevyDb.Cmd"/> never throws for a protocol error — it
/// returns <see cref="KevyValue.Error"/> as a value.</summary>
public sealed class KevyException(string message, Exception? inner = null)
    : Exception(message, inner);

/// <summary>One node of a parsed RESP reply.</summary>
public abstract record KevyValue
{
    public sealed record Simple(string Value) : KevyValue;

    public sealed record Error(string Message) : KevyValue;

    public sealed record Int(long Value) : KevyValue;

    public sealed record Bulk(byte[] Bytes) : KevyValue;

    public sealed record Nil : KevyValue;

    public sealed record Array(IReadOnlyList<KevyValue> Items) : KevyValue;

    public bool IsError => this is Error;

    /// <summary>Bulk / simple payload as UTF-8 text, when that is what it is.</summary>
    public string? AsText => this switch
    {
        Simple s => s.Value,
        Bulk b => Encoding.UTF8.GetString(b.Bytes),
        _ => null,
    };

    public long? AsLong => this is Int i ? i.Value : null;

    /// <summary>Parse one complete RESP reply.</summary>
    public static KevyValue Parse(byte[] reply)
    {
        var pos = 0;
        var v = One(reply, ref pos);
        if (pos != reply.Length) throw new KevyException("kevy: trailing RESP bytes");
        return v;
    }

    private static KevyValue One(ReadOnlySpan<byte> b, ref int pos)
    {
        var head = Line(b, ref pos, out var tag);
        switch (tag)
        {
            case (byte)'+':
                return new Simple(head);
            case (byte)'-':
                return new Error(head);
            case (byte)':':
                return new Int(long.Parse(head));
            case (byte)'$':
            {
                var n = int.Parse(head);
                if (n < 0) return new Nil();
                if (pos + n + 2 > b.Length) throw new KevyException("kevy: truncated RESP reply");
                var bytes = b.Slice(pos, n).ToArray();
                pos += n + 2;
                return new Bulk(bytes);
            }
            case (byte)'*':
            {
                var n = int.Parse(head);
                if (n < 0) return new Nil();
                var items = new List<KevyValue>(n);
                for (var i = 0; i < n; i++) items.Add(One(b, ref pos));
                return new Array(items);
            }
            default:
                throw new KevyException($"kevy: unknown RESP tag {tag}");
        }
    }

    /// <summary>Consume "&lt;tag&gt;head\r\n" at pos, returning head.</summary>
    private static string Line(ReadOnlySpan<byte> b, ref int pos, out byte tag)
    {
        if (pos >= b.Length) throw new KevyException("kevy: truncated RESP reply");
        tag = b[pos];
        var rel = b[(pos + 1)..].IndexOf("\r\n"u8); // vectorized CRLF scan
        if (rel < 0) throw new KevyException("kevy: truncated RESP reply");
        var head = Encoding.UTF8.GetString(b.Slice(pos + 1, rel));
        pos = pos + 1 + rel + 2;
        return head;
    }
}
