// Connect-URL parsing and the process-global embedded registry (contract
// §1.1–§1.3). One connect(url) chooses the backend from the scheme; TLS,
// AUTH, and unknown schemes are rejected before any I/O. Two connects on
// the same mem://<name> or file://<path> resolve to the SAME backing
// store (and pub/sub bus) — modelled here as a URL-keyed, reference-counted
// map that evicts when the last handle drops.

using Kevy.Embedded;

namespace Kevy;

internal enum TargetKind { MemAnon, MemNamed, File, Remote }

internal sealed class Target
{
    public TargetKind Kind { get; init; }
    public string Name { get; init; } = "";   // mem://<name>
    public string Path { get; init; } = "";    // file://<canonical-path>
    public string Host { get; init; } = "";
    public ushort Port { get; init; } = 6379;
    public uint? Db { get; init; }              // remote /db (null = none)
    public string Url { get; init; } = "";

    public string RegistryKey => Kind switch
    {
        TargetKind.MemNamed => "mem://" + Name,
        TargetKind.File => "file://" + Path,
        _ => "",
    };
}

internal static class Url
{
    // Parse resolves a connect URL to a Target, rejecting TLS/AUTH and
    // unknown schemes before any I/O (contract §1.1).
    internal static Target Parse(string url)
    {
        var idx = url.IndexOf("://", StringComparison.Ordinal);
        if (idx < 0) throw new KevyInvalidInputException("URL missing '://'");
        var scheme = url[..idx];
        var rest = url[(idx + 3)..];
        switch (scheme)
        {
            case "mem":
                return rest.Length == 0
                    ? new Target { Kind = TargetKind.MemAnon, Url = url }
                    : new Target { Kind = TargetKind.MemNamed, Name = rest, Url = url };
            case "file":
                if (rest.Length == 0)
                    throw new KevyInvalidInputException(
                        "file:// URL must include a path (e.g. file:///var/lib/myapp)");
                return new Target { Kind = TargetKind.File, Path = System.IO.Path.GetFullPath(rest), Url = url };
            case "kevy" or "redis" or "tcp":
                return ParseRemote(scheme, rest, url);
            case "rediss" or "kevys":
                throw new KevyUnsupportedException(
                    "TLS schemes (rediss://, kevys://) are unsupported — kevy has no TLS");
            default:
                throw new KevyInvalidInputException($"unknown URL scheme '{scheme}://'");
        }
    }

    private static Target ParseRemote(string scheme, string rest, string url)
    {
        if (rest.Contains('@'))
            throw new KevyUnsupportedException(
                "userinfo (user:pass@host) is unsupported — kevy has no AUTH");
        var authority = rest;
        var path = "";
        var slash = rest.IndexOf('/');
        if (slash >= 0) { authority = rest[..slash]; path = rest[(slash + 1)..]; }
        var (host, port) = ParseAuthority(authority);
        uint? db = null;
        // tcp:// is raw: it never carries a db (ignores any /db, no SELECT).
        if (scheme != "tcp" && path.Length > 0)
        {
            if (!uint.TryParse(path, out var n))
                throw new KevyInvalidInputException(
                    $"bad db index: '{path}' (expected a non-negative integer)");
            db = n;
        }
        return new Target { Kind = TargetKind.Remote, Host = host, Port = port, Db = db, Url = url };
    }

    private static (string, ushort) ParseAuthority(string authority)
    {
        var host = authority;
        ushort port = 6379;
        var colon = authority.LastIndexOf(':');
        if (colon >= 0)
        {
            host = authority[..colon];
            if (!ushort.TryParse(authority[(colon + 1)..], out port))
                throw new KevyInvalidInputException("bad port: " + authority[(colon + 1)..]);
        }
        if (host.Length == 0) throw new KevyInvalidInputException("empty host");
        return (host, port);
    }
}

// Registry is the process-global, URL-keyed weak map (contract §1.3). Two
// connects on the same shared embedded URL share one KevyDb (and one bus);
// the store closes when the last reference drops.
internal static class Registry
{
    private sealed class Shared { public required KevyDb Db; public int Refs; }

    private static readonly object Gate = new();
    private static readonly Dictionary<string, Shared> Map = new();

    // Resolve opens (or shares) the embedded store for an embedded target,
    // returning the store plus the registry key to release later.
    internal static (KevyDb Db, string Key) Resolve(Target t)
    {
        var key = t.RegistryKey;
        if (key.Length != 0)
            lock (Gate)
                if (Map.TryGetValue(key, out var s)) { s.Refs++; return (s.Db, key); }

        var db = Open(t);
        if (key.Length == 0) return (db, "");

        lock (Gate)
        {
            // Lost a race: another thread registered first — use theirs.
            if (Map.TryGetValue(key, out var s)) { s.Refs++; db.Dispose(); return (s.Db, key); }
            Map[key] = new Shared { Db = db, Refs = 1 };
            return (db, key);
        }
    }

    private static KevyDb Open(Target t) => t.Kind switch
    {
        TargetKind.MemAnon or TargetKind.MemNamed => KevyDb.OpenInMemory(),
        TargetKind.File => KevyDb.Open(t.Path),
        _ => throw new KevyInvalidInputException("Resolve called on a non-embedded target"),
    };

    // Release drops one reference, closing the store when the last goes.
    // Anonymous stores (empty key) close directly.
    internal static void Release(string key, KevyDb db)
    {
        if (key.Length == 0) { db.Dispose(); return; }
        lock (Gate)
        {
            if (!Map.TryGetValue(key, out var s)) return;
            if (--s.Refs > 0) return;
            s.Db.Dispose();
            Map.Remove(key);
        }
    }
}
