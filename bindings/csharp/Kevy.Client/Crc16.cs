// CRC16-CCITT (XMODEM) and the Redis-cluster key→slot mapping (contract
// §3.15 / §7). Reproduces Redis's key_hash_slot exactly so client routing
// agrees with the server and -MOVED never fires for a correctly-routed
// key. Check vector: crc16("123456789") == 0x31C3.

namespace Kevy;

/// <summary>Redis-cluster hashing (16384 slots + {hashtag} extraction).</summary>
public static class Crc16
{
    private const ushort Poly = 0x1021;
    private static readonly ushort[] Table = MakeTable();

    private static ushort[] MakeTable()
    {
        var t = new ushort[256];
        for (var i = 0; i < 256; i++)
        {
            var crc = (ushort)(i << 8);
            for (var bit = 0; bit < 8; bit++)
                crc = (crc & 0x8000) != 0 ? (ushort)((crc << 1) ^ Poly) : (ushort)(crc << 1);
            t[i] = crc;
        }
        return t;
    }

    /// <summary>CRC16-CCITT (XMODEM) over the bytes.</summary>
    public static ushort Hash(ReadOnlySpan<byte> b)
    {
        ushort crc = 0;
        foreach (var c in b)
            crc = (ushort)((crc << 8) ^ Table[(byte)(crc >> 8) ^ c]);
        return crc;
    }

    /// <summary>The Redis-cluster hash slot of key: crc16(hashtag(key)) &amp; 16383.</summary>
    public static ushort KeyHashSlot(ReadOnlySpan<byte> key) => (ushort)(Hash(HashTag(key)) & 0x3FFF);

    // HashTag hashes only the bytes between the first '{' and the first '}'
    // after it, when that span is non-empty; otherwise the whole key.
    private static ReadOnlySpan<byte> HashTag(ReadOnlySpan<byte> key)
    {
        var open = key.IndexOf((byte)'{');
        if (open < 0) return key;
        var rest = key[(open + 1)..];
        var close = rest.IndexOf((byte)'}');
        if (close < 0) return key;
        return close > 0 ? rest[..close] : key;
    }
}
