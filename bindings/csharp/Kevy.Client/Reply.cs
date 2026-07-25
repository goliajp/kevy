// The universal decoded RESP value + the RESP2/RESP3 parser and the
// multibulk command encoder (contract §4.1). One Reply type covers every
// RESP2 and RESP3 shape (RESP3 push/pubsub and the typed extension replies
// need them all). The parser speaks RESP2 + RESP3, transparently discards
// attribute frames, and is fuzz-hardened (a hostile length header cannot
// force a huge allocation). It parses one complete reply from a
// self-contained buffer and reports bytes consumed so the streaming TCP
// reader can reassemble across socket reads (0 consumed = need more bytes).

using System.Buffers.Text;
using System.Text;

namespace Kevy;

/// <summary>Tags one node of a decoded RESP reply. RESP2 uses the first
/// six; RESP3 adds the rest (contract §4.1).</summary>
public enum ReplyKind
{
    /// <summary>+OK-style simple string.</summary>
    Simple,
    /// <summary>-ERR error frame (text without the leading '-').</summary>
    Error,
    /// <summary>:N integer.</summary>
    Int,
    /// <summary>$len bulk string.</summary>
    Bulk,
    /// <summary>$-1 / *-1 RESP2 null.</summary>
    Nil,
    /// <summary>*N array.</summary>
    Array,
    /// <summary>%N RESP3 map.</summary>
    Map,
    /// <summary>~N RESP3 set.</summary>
    Set,
    /// <summary>,N RESP3 double.</summary>
    Double,
    /// <summary>#t/#f RESP3 boolean.</summary>
    Boolean,
    /// <summary>=N RESP3 verbatim string.</summary>
    Verbatim,
    /// <summary>(N RESP3 big number (raw digit bytes).</summary>
    BigNumber,
    /// <summary>_ RESP3 true null.</summary>
    Null,
    /// <summary>&gt;N RESP3 out-of-band push frame.</summary>
    Push,
    /// <summary>!N RESP3 length-prefixed error.</summary>
    BlobError,
}

/// <summary>One decoded RESP value — the universal reply type covering all
/// RESP2 and RESP3 shapes (contract §4.1).</summary>
public sealed class Reply
{
    /// <summary>The RESP shape of this node.</summary>
    public required ReplyKind Kind { get; init; }
    /// <summary>Payload for Simple / Error / Bulk / BigNumber / BlobError,
    /// and the data body for Verbatim.</summary>
    public byte[] Bytes { get; init; } = [];
    /// <summary>Payload for Int.</summary>
    public long Integer { get; init; }
    /// <summary>Payload for Double.</summary>
    public double Double { get; init; }
    /// <summary>Payload for Boolean.</summary>
    public bool Bool { get; init; }
    /// <summary>The 3-char format tag for Verbatim.</summary>
    public byte[] VerbatimFmt { get; init; } = [];
    /// <summary>Elements for Array / Set / Push.</summary>
    public IReadOnlyList<Reply> Items { get; init; } = [];
    /// <summary>Key/value pairs for Map.</summary>
    public IReadOnlyList<(Reply Key, Reply Val)> Pairs { get; init; } = [];

    /// <summary>Whether the engine answered with a protocol error.</summary>
    public bool IsError => Kind is ReplyKind.Error or ReplyKind.BlobError;

    /// <summary>Whether the reply is a RESP2 nil or RESP3 null.</summary>
    public bool IsNil => Kind is ReplyKind.Nil or ReplyKind.Null;

    /// <summary>The payload as UTF-8 text (Simple / Error / Bulk / Verbatim
    /// / BigNumber / BlobError), or "" for other kinds.</summary>
    public string Str() => Encoding.UTF8.GetString(Bytes);

    internal string Shape() => Kind switch
    {
        ReplyKind.Simple => "simple-string",
        ReplyKind.Error => "error",
        ReplyKind.Int => "integer",
        ReplyKind.Bulk => "bulk-string",
        ReplyKind.Nil or ReplyKind.Null => "nil",
        ReplyKind.Array => "array",
        ReplyKind.Map => "map",
        ReplyKind.Set => "set",
        ReplyKind.Double => "double",
        ReplyKind.Boolean => "boolean",
        ReplyKind.Verbatim => "verbatim-string",
        ReplyKind.BigNumber => "big-number",
        ReplyKind.Push => "push",
        ReplyKind.BlobError => "blob-error",
        _ => "unknown",
    };

    /// <summary>Parse exactly one complete reply from a self-contained
    /// buffer (the embedded FFI path). Throws on a truncated/malformed
    /// frame.</summary>
    public static Reply Decode(ReadOnlySpan<byte> buf)
    {
        var used = Resp.Parse(buf, out var reply);
        if (used == 0 || reply is null)
            throw new KevyProtocolException("truncated RESP reply");
        return reply;
    }
}

/// <summary>The RESP2/RESP3 wire codec.</summary>
internal static class Resp
{
    // EncodedSize is the exact byte length EncodeInto will write for argv, so
    // the caller can rent one buffer of that size (no growth, no re-copy).
    internal static int EncodedSize(IReadOnlyList<byte[]> argv)
    {
        var n = 3 + DigitCount(argv.Count); // '*' + digits + CRLF
        foreach (var a in argv)
            n += 5 + DigitCount(a.Length) + a.Length; // '$' + digits + CRLF + data + CRLF
        return n;
    }

    // EncodeInto writes argv as a RESP multibulk request into dst (which must
    // hold at least EncodedSize bytes), returning the count written. Integer
    // headers format straight into the buffer via Utf8Formatter — no per-arg
    // string or byte[] allocation.
    internal static int EncodeInto(Span<byte> dst, IReadOnlyList<byte[]> argv)
    {
        var p = 0;
        dst[p++] = (byte)'*';
        p += WriteUInt(dst[p..], argv.Count);
        p = Crlf(dst, p);
        foreach (var a in argv)
        {
            dst[p++] = (byte)'$';
            p += WriteUInt(dst[p..], a.Length);
            p = Crlf(dst, p);
            a.CopyTo(dst[p..]);
            p += a.Length;
            p = Crlf(dst, p);
        }
        return p;
    }

    private static int Crlf(Span<byte> dst, int p)
    {
        dst[p] = (byte)'\r';
        dst[p + 1] = (byte)'\n';
        return p + 2;
    }

    private static int WriteUInt(Span<byte> dst, long v)
    {
        Utf8Formatter.TryFormat(v, dst, out var written);
        return written;
    }

    private static int DigitCount(long v)
    {
        var n = 1;
        for (var t = v; t >= 10; t /= 10) n++;
        return n;
    }

    // Parse decodes one RESP reply from the front of buf, returning the
    // bytes consumed (0 = need more). Throws KevyProtocolException on a
    // malformed frame. Speaks RESP2 + RESP3 and skips attribute frames.
    internal static int Parse(ReadOnlySpan<byte> buf, out Reply? reply)
    {
        reply = null;
        if (buf.Length == 0) return 0;
        return (char)buf[0] switch
        {
            '+' => Line(buf, ReplyKind.Simple, out reply),
            '-' => Line(buf, ReplyKind.Error, out reply),
            ':' => IntReply(buf, out reply),
            '$' => BulkReply(buf, ReplyKind.Bulk, out reply),
            '*' => AggReply(buf, ReplyKind.Array, out reply),
            '%' => MapReply(buf, out reply),
            '~' => AggReply(buf, ReplyKind.Set, out reply),
            ',' => DoubleReply(buf, out reply),
            '#' => BoolReply(buf, out reply),
            '=' => VerbatimReply(buf, out reply),
            '(' => Line(buf, ReplyKind.BigNumber, out reply),
            '_' => NullReply(buf, out reply),
            '>' => AggReply(buf, ReplyKind.Push, out reply),
            '!' => BulkReply(buf, ReplyKind.BlobError, out reply),
            '|' => Attributed(buf, out reply),
            _ => throw new KevyProtocolException($"unknown RESP tag {(char)buf[0]}"),
        };
    }

    // First CRLF at or after `from` (hardware-vectorized two-byte search), or
    // -1 if the frame is not yet complete.
    private static int FindCrlf(ReadOnlySpan<byte> buf, int from)
    {
        var rel = buf[from..].IndexOf("\r\n"u8);
        return rel < 0 ? -1 : from + rel;
    }

    private static int Line(ReadOnlySpan<byte> buf, ReplyKind kind, out Reply? reply)
    {
        reply = null;
        var eol = FindCrlf(buf, 1);
        if (eol < 0) return 0;
        reply = new Reply { Kind = kind, Bytes = buf[1..eol].ToArray() };
        return eol + 2;
    }

    private static int IntReply(ReadOnlySpan<byte> buf, out Reply? reply)
    {
        reply = null;
        var eol = FindCrlf(buf, 1);
        if (eol < 0) return 0;
        if (!ParseI64(buf[1..eol], out var n))
            throw new KevyProtocolException("bad integer reply");
        reply = new Reply { Kind = ReplyKind.Int, Integer = n };
        return eol + 2;
    }

    private static int BulkReply(ReadOnlySpan<byte> buf, ReplyKind kind, out Reply? reply)
    {
        reply = null;
        var hdr = FindCrlf(buf, 1);
        if (hdr < 0) return 0;
        if (!ParseI32(buf[1..hdr], out var n))
            throw new KevyProtocolException("bad bulk length");
        if (n < 0) { reply = new Reply { Kind = ReplyKind.Nil }; return hdr + 2; }
        var start = hdr + 2;
        var end = start + n;
        if (buf.Length < end + 2) return 0;
        reply = new Reply { Kind = kind, Bytes = buf[start..end].ToArray() };
        return end + 2;
    }

    private static int AggReply(ReadOnlySpan<byte> buf, ReplyKind kind, out Reply? reply)
    {
        reply = null;
        var hdr = FindCrlf(buf, 1);
        if (hdr < 0) return 0;
        if (!ParseI32(buf[1..hdr], out var count))
            throw new KevyProtocolException("bad aggregate length");
        if (count < 0)
        {
            if (kind == ReplyKind.Push) throw new KevyProtocolException("push frame cannot be null");
            reply = new Reply { Kind = ReplyKind.Nil };
            return hdr + 2;
        }
        var pos = hdr + 2;
        var items = new List<Reply>(CapHint(count, buf.Length - pos));
        for (var i = 0; i < count; i++)
        {
            var used = Parse(buf[pos..], out var item);
            if (used == 0 || item is null) return 0;
            items.Add(item);
            pos += used;
        }
        reply = new Reply { Kind = kind, Items = items };
        return pos;
    }

    private static int MapReply(ReadOnlySpan<byte> buf, out Reply? reply)
    {
        reply = null;
        var hdr = FindCrlf(buf, 1);
        if (hdr < 0) return 0;
        if (!ParseI32(buf[1..hdr], out var count) || count < 0)
            throw new KevyProtocolException("bad map length");
        var pos = hdr + 2;
        var pairs = new List<(Reply, Reply)>(CapHint(count, (buf.Length - pos) / 2));
        for (var i = 0; i < count; i++)
        {
            var ku = Parse(buf[pos..], out var k);
            if (ku == 0 || k is null) return 0;
            pos += ku;
            var vu = Parse(buf[pos..], out var v);
            if (vu == 0 || v is null) return 0;
            pos += vu;
            pairs.Add((k, v));
        }
        reply = new Reply { Kind = ReplyKind.Map, Pairs = pairs };
        return pos;
    }

    private static int DoubleReply(ReadOnlySpan<byte> buf, out Reply? reply)
    {
        reply = null;
        var eol = FindCrlf(buf, 1);
        if (eol < 0) return 0;
        var s = buf[1..eol];
        if (!Utf8Parser.TryParse(s, out double v, out var used) || used != s.Length)
            throw new KevyProtocolException("bad double");
        reply = new Reply { Kind = ReplyKind.Double, Double = v };
        return eol + 2;
    }

    private static int BoolReply(ReadOnlySpan<byte> buf, out Reply? reply)
    {
        reply = null;
        var eol = FindCrlf(buf, 1);
        if (eol < 0) return 0;
        var body = buf[1..eol];
        if (body.Length != 1) throw new KevyProtocolException("bad boolean payload");
        reply = body[0] switch
        {
            (byte)'t' => new Reply { Kind = ReplyKind.Boolean, Bool = true },
            (byte)'f' => new Reply { Kind = ReplyKind.Boolean, Bool = false },
            _ => throw new KevyProtocolException("bad boolean payload"),
        };
        return eol + 2;
    }

    private static int VerbatimReply(ReadOnlySpan<byte> buf, out Reply? reply)
    {
        reply = null;
        var hdr = FindCrlf(buf, 1);
        if (hdr < 0) return 0;
        if (!ParseI32(buf[1..hdr], out var n))
            throw new KevyProtocolException("bad verbatim length");
        if (n < 4) throw new KevyProtocolException("verbatim length < 4 (fmt + ':')");
        var start = hdr + 2;
        var end = start + n;
        if (buf.Length < end + 2) return 0;
        var body = buf[start..end];
        if (body[3] != (byte)':') throw new KevyProtocolException("verbatim missing fmt:data separator");
        reply = new Reply { Kind = ReplyKind.Verbatim, VerbatimFmt = body[..3].ToArray(), Bytes = body[4..].ToArray() };
        return end + 2;
    }

    private static int NullReply(ReadOnlySpan<byte> buf, out Reply? reply)
    {
        reply = null;
        if (buf.Length < 3) return 0;
        if (buf[0] != (byte)'_' || buf[1] != (byte)'\r' || buf[2] != (byte)'\n')
            throw new KevyProtocolException("bad null payload");
        reply = new Reply { Kind = ReplyKind.Null };
        return 3;
    }

    // Attributed consumes a |N attribute map and returns the reply it
    // decorates (contract §4.1: attributes are transparently discarded).
    private static int Attributed(ReadOnlySpan<byte> buf, out Reply? reply)
    {
        reply = null;
        var used = MapReply(buf, out _);
        if (used == 0) return 0;
        var u2 = Parse(buf[used..], out reply);
        if (u2 == 0) { reply = null; return 0; }
        return used + u2;
    }

    // CapHint caps a header-declared count by the bytes remaining so a
    // hostile length header cannot force a huge allocation (fuzz-hardened).
    private static int CapHint(int count, int remaining)
    {
        if (remaining < 0) remaining = 0;
        return count > remaining ? remaining : count;
    }

    // Zero-alloc header integer parse (§4.1). Utf8Parser consumes the digits
    // in place; requiring FULL consumption keeps the original strictness (a
    // trailing non-digit like ":12x\r\n" is still rejected).
    private static bool ParseI64(ReadOnlySpan<byte> b, out long value) =>
        Utf8Parser.TryParse(b, out value, out var used) && used == b.Length;

    private static bool ParseI32(ReadOnlySpan<byte> b, out int value) =>
        Utf8Parser.TryParse(b, out value, out var used) && used == b.Length;
}
