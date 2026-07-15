// Enums shared with the wire protocol and the embedded store (contract
// §4.13 / §4.2). Wire tags are emitted only for the non-default cases so
// the request bytes match the Rust reference and the other ports.

using System.Text;

namespace Kevy;

/// <summary>AGGREGATE mode for ZINTERSTORE / ZUNIONSTORE (§3.6). The
/// default, <see cref="Sum"/>, emits no wire tag.</summary>
public enum ZAggregate { Sum, Min, Max }

/// <summary>Optional condition on the hash field-TTL verbs (§3.7).
/// <see cref="Always"/> emits no wire keyword.</summary>
public enum HExpireCond { Always, Nx, Xx, Gt, Lt }

/// <summary>The declared scalar type of a range index (§4.2).</summary>
public enum IdxType { I64, F64, Str }

/// <summary>Per-connection RESP dialect (§4.1). RESP2 is the default;
/// RESP3 adds the nine extra prefixes plus push frames.</summary>
public enum RespVersion { V2, V3 }

/// <summary>The embedded store's maxmemory behaviour (§5.3).</summary>
public enum EvictionPolicy
{
    NoEviction, AllKeysLru, AllKeysLfu, AllKeysRandom,
    VolatileLru, VolatileLfu, VolatileRandom, VolatileTtl,
}

/// <summary>Tags a <see cref="PubsubEvent"/> (§4.8).</summary>
public enum PubsubKind { Subscribe, Psubscribe, Unsubscribe, Punsubscribe, Message, Pmessage }

internal static class WireTags
{
    internal static byte[] Tag(this ZAggregate a) => a switch
    {
        ZAggregate.Min => "MIN"u8.ToArray(),
        ZAggregate.Max => "MAX"u8.ToArray(),
        _ => "SUM"u8.ToArray(),
    };

    internal static byte[]? Keyword(this HExpireCond c) => c switch
    {
        HExpireCond.Nx => "NX"u8.ToArray(),
        HExpireCond.Xx => "XX"u8.ToArray(),
        HExpireCond.Gt => "GT"u8.ToArray(),
        HExpireCond.Lt => "LT"u8.ToArray(),
        _ => null,
    };

    internal static byte[] Tag(this IdxType t) => t switch
    {
        IdxType.F64 => "f64"u8.ToArray(),
        IdxType.Str => "str"u8.ToArray(),
        _ => "i64"u8.ToArray(),
    };
}
