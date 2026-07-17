// KevyOpenOptions — the explicit open policy, .NET-shaped.

namespace Kevy.Embedded;

/// <summary>The AOF fsync policy for <see cref="KevyDb.OpenWith"/>.</summary>
public enum KevyFsync : byte
{
    /// <summary>Fsync once a second (the default — Redis appendfsync everysec).</summary>
    EverySec = 0,

    /// <summary>Fsync on every write.</summary>
    Always = 1,

    /// <summary>Never fsync; the OS decides.</summary>
    No = 2,
}

/// <summary>The explicit open policy <see cref="KevyDb.OpenWith"/> takes —
/// the knobs <see cref="KevyDb.Open"/> locks to defaults. The property
/// defaults are the C header's KEVY_OPEN_OPTIONS_INIT, i.e. exactly what
/// a plain Open uses; override what you need.
/// <see cref="RewriteBytes"/> and <see cref="RewriteIntervalSecs"/> are
/// the absolute-size and staleness auto-rewrite triggers (0 = off).</summary>
public sealed record KevyOpenOptions
{
    /// <summary>AOF fsync policy.</summary>
    public KevyFsync Fsync { get; init; } = KevyFsync.EverySec;

    /// <summary>Keyspace shards (0 = default, 1).</summary>
    public uint Shards { get; init; }

    /// <summary>Auto-rewrite growth trigger, percent (0 = rule off).</summary>
    public uint RewritePct { get; init; } = 100;

    /// <summary>Growth rule's minimum size gate.</summary>
    public ulong RewriteMinSize { get; init; } = 64UL * 1024 * 1024;

    /// <summary>Absolute-size trigger (0 = rule off).</summary>
    public ulong RewriteBytes { get; init; }

    /// <summary>Staleness trigger, seconds (0 = rule off).</summary>
    public ulong RewriteIntervalSecs { get; init; }
}
