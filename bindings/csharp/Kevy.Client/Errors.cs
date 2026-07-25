// The KevyException hierarchy — the C# mapping of the canonical error
// taxonomy (contract §2). Every typed method throws a KevyException
// subclass; the variant identity survives the mapping (a caller can
// distinguish a store wrong-type from a transport failure) because the
// conformance tests assert on variant. A server error *reply* whose text
// is a recognized store-semantic error surfaces as KevyStoreException on
// BOTH backends; any other -ERR is a KevyProtocolException with the wire
// text preserved verbatim.

namespace Kevy;

/// <summary>Structured store-semantic errors, carried by
/// <see cref="KevyStoreException"/> (contract §2.3). These stay structured,
/// never stringly.</summary>
public enum StoreError
{
    /// <summary>Key holds a different type than the command expects (WRONGTYPE).</summary>
    WrongType,
    /// <summary>Value is not a base-10 integer (INCR family).</summary>
    NotInteger,
    /// <summary>The result would overflow i64.</summary>
    Overflow,
    /// <summary>Index outside the collection (LSET).</summary>
    OutOfRange,
    /// <summary>Key does not exist where the command requires one (LSET).</summary>
    NoSuchKey,
    /// <summary>Value is not a valid float (INCRBYFLOAT).</summary>
    NotFloat,
    /// <summary>maxmemory exceeded under NoEviction (OOM).</summary>
    OutOfMemory,
}

/// <summary>Base of the kevy client error hierarchy (contract §2.2). Catch
/// this to handle any kevy failure; catch a subclass to branch on variant.</summary>
public abstract class KevyException : Exception
{
    private protected KevyException(string message, Exception? inner = null)
        : base(message, inner) { }
}

/// <summary>A structured store-semantic error (WRONGTYPE, non-integer, …),
/// recognized on both backends. Inspect <see cref="Error"/>.</summary>
public sealed class KevyStoreException(StoreError error)
    : KevyException("store error: " + error) { public StoreError Error { get; } = error; }

/// <summary>An OS/transport failure — socket, file, or AOF I/O.</summary>
public sealed class KevyIoException(string message, Exception? cause = null)
    : KevyException("io error: " + message, cause) { }

/// <summary>A RESP-level error: a server -ERR reply (text preserved
/// verbatim in <see cref="Text"/>) or a malformed / unexpected reply
/// shape.</summary>
public sealed class KevyProtocolException(string text)
    : KevyException("protocol error: " + text) { public string Text { get; } = text; }

/// <summary>The write was rejected by a read-only replica.</summary>
public sealed class KevyReadOnlyException()
    : KevyException("READONLY You can't write against a read only replica") { }

/// <summary>A bad argument to a typed API — rejected before touching state.</summary>
public sealed class KevyInvalidInputException(string message)
    : KevyException("invalid input: " + message) { }

/// <summary>A named object (index, view, key) does not exist.</summary>
public sealed class KevyNotFoundException(string message)
    : KevyException("not found: " + message) { }

/// <summary>The op is unavailable on this backend/build (IDX.* on
/// embedded, TLS, AUTH, …).</summary>
public sealed class KevyUnsupportedException(string message)
    : KevyException("unsupported: " + message) { }

/// <summary>A bounded blocking call ran out its timeout.</summary>
public sealed class KevyTimedOutException()
    : KevyException("timed out") { }

/// <summary>The connection / in-process bus is gone (EOF).</summary>
public sealed class KevyClosedException()
    : KevyException("connection closed") { }

internal static class Err
{
    // classifyStoreError maps a server error frame's text onto the
    // structured taxonomy (contract §2.2). Matches the Go/Python ports.
    internal static bool ClassifyStore(string text, out StoreError kind)
    {
        kind = StoreError.WrongType;
        if (text.StartsWith("WRONGTYPE", StringComparison.Ordinal)) { kind = StoreError.WrongType; return true; }
        if (text.StartsWith("OOM", StringComparison.Ordinal)) { kind = StoreError.OutOfMemory; return true; }
        if (text.Contains("is not an integer")) { kind = StoreError.NotInteger; return true; }
        if (text.Contains("is not a valid float")) { kind = StoreError.NotFloat; return true; }
        if (text.Contains("would overflow")) { kind = StoreError.Overflow; return true; }
        if (text.Contains("no such key")) { kind = StoreError.NoSuchKey; return true; }
        if (text.Contains("index out of range")) { kind = StoreError.OutOfRange; return true; }
        return false;
    }

    // ReplyError converts a Reply::Error into the right exception: a
    // recognized store error → KevyStoreException; otherwise the wire text
    // verbatim → KevyProtocolException (contract §2.2).
    internal static KevyException ReplyError(Reply r)
    {
        var text = r.Str();
        return ClassifyStore(text, out var kind)
            ? new KevyStoreException(kind)
            : new KevyProtocolException(text);
    }

    internal static KevyProtocolException Unexpected(Reply r) =>
        new("unexpected RESP reply variant: " + r.Shape());
}
