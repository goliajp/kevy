// Public data types (contract §4). All key/value/member payloads are raw
// bytes (binary-safe); the client never assumes UTF-8 for user data. The
// hash field-TTL codes are signed 8-bit (sbyte), kept an integer not a
// bool. These are plain records — value equality helps the tests assert.

namespace Kevy;

/// <summary>A (score, member) pair for ZADD (§3.5).</summary>
public readonly record struct ZMember(double Score, byte[] Member);

/// <summary>One IDX.QUERY hit: the row key plus the indexed value's wire
/// string form (§4.3).</summary>
public sealed record IdxRow(byte[] Key, byte[] Value);

/// <summary>One page of IDX.QUERY RANGE/EQ results (§4.4). <see cref="Cursor"/>
/// is null when the scan is complete (wire "0"); else pass it back to
/// resume.</summary>
public sealed record IdxPage(byte[]? Cursor, IReadOnlyList<IdxRow> Rows);

/// <summary>One IDX.LIST entry (§4.5). Parsed from a flat label/value
/// array; unknown labels are skipped (forward-compatible).</summary>
public sealed record IdxInfo(
    byte[] Name, byte[] Prefix, string Kind, string State, ulong Entries, ulong Bytes);

/// <summary>One (key, score/distance) result from MATCH / KNN (§3.8).</summary>
public sealed record Ranked(byte[] Key, double Score);

/// <summary>One applied effect's argv at a stream offset (§4.6).</summary>
public sealed record FeedFrame(ulong Offset, IReadOnlyList<byte[]> Argv);

/// <summary>A run of frames plus the cursor to resume from (§4.7). Resume
/// the next FeedRead from (<see cref="Generation"/>, <see cref="NextOffset"/>).</summary>
public sealed record FeedBatch(ulong Generation, ulong NextOffset, IReadOnlyList<FeedFrame> Frames);

/// <summary>One received pub/sub frame — acks and deliveries alike (§4.8).
/// <see cref="Count"/> is the total channels + patterns held after the op;
/// for Unsubscribe/Punsubscribe a null Channel/Pattern means "all".</summary>
public sealed record PubsubEvent(
    PubsubKind Kind, byte[]? Channel, byte[]? Pattern, byte[]? Payload, long Count);

/// <summary>A (key, value) blocking-pop hit (§3.14).</summary>
public readonly record struct Kv(byte[] Key, byte[] Value);

/// <summary>A (key, member, score) BZPOPMIN hit (§4.9).</summary>
public readonly record struct ZPopHit(byte[] Key, byte[] Member, double Score);
