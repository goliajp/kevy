// KevyOpenStats — the boot-replay verdict, .NET-shaped.

namespace Kevy.Embedded;

/// <summary>The boot-replay verdict <see cref="KevyDb.OpenReport"/> returns —
/// the typed face of the C ABI's KevyOpenReport.</summary>
public sealed record KevyOpenStats
{
    /// <summary>Commands replayed from the AOF(s), summed across shards.</summary>
    public required ulong ReplayedCommands { get; init; }

    /// <summary>Bytes actually replayed (the valid prefixes).</summary>
    public required ulong ReplayedBytes { get; init; }

    /// <summary>Wall-clock time of the startup replay, in milliseconds.</summary>
    public required ulong ElapsedMs { get; init; }

    /// <summary>Bytes dropped past the last replayable frame (quarantined
    /// on disk).</summary>
    public required ulong DroppedBytes { get; init; }

    /// <summary>True when any shard's replay stopped at a corrupt frame.</summary>
    public required bool Corrupt { get; init; }

    /// <summary>Quarantine files written by the open's repair.</summary>
    public required uint QuarantineCount { get; init; }
}
