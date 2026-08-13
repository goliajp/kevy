// KevyDb — kevy embedded, .NET-shaped.
//
// The typed methods mirror the other kevy packages (Set/Get/Del/IncrBy/…);
// Cmd() reaches every verb. Typed methods throw KevyException on a
// protocol error — a typed call has one meaning, so "-WRONGTYPE" is a
// failure. Cmd() returns KevyValue.Error as a VALUE for callers driving
// the raw verb surface.
//
//   using var db = KevyDb.Open("data/");
//   db.Set("session:7f3a", value, ttlMs: 3_600_000);
//   db.GetText("session:7f3a");

using System.Runtime.InteropServices;
using System.Text;

namespace Kevy.Embedded;

/// <summary>The embedded engine. One instance owns its data directory.</summary>
public sealed unsafe class KevyDb : IDisposable
{
    // The C ABI version (kevy-ffi KEVY_ABI) this assembly's P/Invoke
    // signatures target. Asserted against the loaded cdylib on open.
    private const uint ExpectedAbi = 1;

    private IntPtr _db;
    private readonly List<KevySubscription> _subs = [];

    private KevyDb(IntPtr db) => _db = db;

    // Fail fast when the loaded engine speaks a different ABI than these
    // signatures were compiled against — otherwise a mismatch is silent
    // memory corruption at the first call.
    private static void CheckAbi()
    {
        uint actual;
        try
        {
            // Every embedded entry point comes through here, so this is the
            // one place the engine's absence can be turned into a sentence
            // that names the fix. The runtime's own DllNotFoundException
            // says "Unable to load shared library 'kevy_ffi'" and leaves a
            // reader to guess whether they typed something wrong, are on an
            // unsupported platform, or are missing a step. Not a caught
            // failure being swallowed — it is rethrown, with the answer.
            actual = KevyNative.kevy_abi();
        }
        catch (DllNotFoundException e)
        {
            throw new KevyException(
                "kevy: no embedded engine — the in-process store needs " +
                "libkevy_ffi, a per-platform native library this package " +
                "does not carry. Either set KEVY_FFI_LIB to one, or build " +
                "it from the engine source: git clone " +
                "https://github.com/goliajp/kevy && cargo build --release " +
                "-p kevy-ffi (it lands in target/release/). A kevy:// or " +
                "redis:// URL needs none of this — that client is managed " +
                "code only.", e);
        }
        if (actual != ExpectedAbi)
            throw new KevyException(
                $"kevy: ABI mismatch — this client targets kevy_abi {ExpectedAbi}, " +
                $"the loaded engine reports {actual}; update the kevy native library or the client");
    }

    /// <summary>Open a persistent store rooted at <paramref name="dir"/>,
    /// replaying its log.</summary>
    public static KevyDb Open(string dir)
    {
        CheckAbi();
        var bytes = Encoding.UTF8.GetBytes(dir);
        IntPtr db;
        fixed (byte* p = bytes)
        {
            db = KevyNative.kevy_open(p, (nuint)bytes.Length);
        }
        return db == IntPtr.Zero ? throw new KevyException("kevy: open failed") : new KevyDb(db);
    }

    /// <summary>Open a pure in-memory store — nothing survives the process.</summary>
    public static KevyDb OpenInMemory()
    {
        CheckAbi();
        var db = KevyNative.kevy_open_mem();
        return db == IntPtr.Zero ? throw new KevyException("kevy: open failed") : new KevyDb(db);
    }

    /// <summary>Open with an explicit policy — the knobs <see cref="Open"/>
    /// locks to defaults: durable at <paramref name="dir"/> when non-null and
    /// non-empty, in-memory when null or "". A null <paramref name="options"/>
    /// behaves exactly like <see cref="Open"/> / <see cref="OpenInMemory"/>.</summary>
    public static KevyDb OpenWith(string? dir, KevyOpenOptions? options = null)
    {
        CheckAbi();
        var bytes = string.IsNullOrEmpty(dir) ? null : Encoding.UTF8.GetBytes(dir);
        var native = options is null
            ? default
            : new KevyOpenOptionsNative
            {
                Fsync = (byte)options.Fsync,
                Shards = options.Shards,
                RewritePct = options.RewritePct,
                RewriteMinSize = options.RewriteMinSize,
                RewriteBytes = options.RewriteBytes,
                RewriteIntervalSecs = options.RewriteIntervalSecs,
            };
        IntPtr db;
        fixed (byte* p = bytes) // fixed on null yields a null pointer
        {
            db = KevyNative.kevy_open_with(
                p, (nuint)(bytes?.Length ?? 0), options is null ? null : &native);
        }
        return db == IntPtr.Zero ? throw new KevyException("kevy: open failed") : new KevyDb(db);
    }

    /// <summary>The engine version.</summary>
    public static string Version()
    {
        var p = KevyNative.kevy_version();
        var len = 0;
        while (p[len] != 0) len++;
        return Encoding.UTF8.GetString(p, len);
    }

    /// <summary>The C ABI version this assembly was written against.</summary>
    public static uint Abi() => KevyNative.kevy_abi();

    private IntPtr Live() =>
        _db == IntPtr.Zero ? throw new KevyException("kevy: closed handle") : _db;

    // ── the escape hatch: every verb, RESP semantics, errors as values ──

    /// <summary>Run one command from UTF-8 string arguments.</summary>
    public KevyValue Cmd(params string[] argv) =>
        CmdBytes([.. argv.Select(Encoding.UTF8.GetBytes)]);

    /// <summary>Cmd for binary-safe arguments.</summary>
    public KevyValue CmdBytes(IReadOnlyList<byte[]> argv) =>
        KevyValue.Parse(CmdRaw(argv));

    /// <summary>Run one command and return the raw RESP reply bytes,
    /// unparsed — the universal command path the unified Kevy client backs
    /// its embedded exec on, parsing with its own RESP3-capable reader.</summary>
    public byte[] CmdRaw(IReadOnlyList<byte[]> argv)
    {
        var db = Live();
        if (argv.Count == 0) throw new KevyException("kevy: empty argv");
        // Pin every argument for the duration of the call; the engine copies
        // on its side. Pin handles live on the stack for the common small
        // argc, falling back to the heap only for a very wide command so a
        // hostile/huge argc can't blow the stack.
        const int StackMax = 128;
        Span<GCHandle> pins = argv.Count > StackMax
            ? new GCHandle[argv.Count]
            : stackalloc GCHandle[argv.Count];
        var ptrs = stackalloc byte*[argv.Count];
        var lens = stackalloc nuint[argv.Count];
        try
        {
            for (var i = 0; i < argv.Count; i++)
            {
                pins[i] = GCHandle.Alloc(argv[i], GCHandleType.Pinned);
                ptrs[i] = (byte*)pins[i].AddrOfPinnedObject();
                lens[i] = (nuint)argv[i].Length;
            }
            KevyBuf @out;
            var rc = KevyNative.kevy_cmd(db, (nuint)argv.Count, ptrs, lens, &@out);
            if (rc != 0) throw new KevyException("kevy: kevy_cmd misuse");
            return KevyNative.TakeBuf(in @out);
        }
        finally
        {
            foreach (var pin in pins)
            {
                if (pin.IsAllocated) pin.Free();
            }
        }
    }

    /// <summary>Open a low-level subscription over one channel or glob
    /// pattern, yielding raw RESP frame bytes via poll / blocking wait —
    /// the surface the unified client's embedded Subscriber is built on
    /// (contract §5.2). Caller owns the returned handle.</summary>
    public KevyRawSub OpenRawSub(byte[] name, bool pattern)
    {
        IntPtr h;
        fixed (byte* p = name.Length > 0 ? name : NonEmpty)
        {
            h = pattern
                ? KevyNative.kevy_psubscribe(Live(), p, (nuint)name.Length)
                : KevyNative.kevy_subscribe(Live(), p, (nuint)name.Length);
        }
        return h == IntPtr.Zero
            ? throw new KevyException("kevy: subscribe failed")
            : new KevyRawSub(h);
    }

    // ── the scalar fast path: no argv assembly, no RESP framing ─────────

    private static readonly byte[] NonEmpty = new byte[1]; // fixed on [] yields null

    /// <summary>Store <paramref name="value"/> under <paramref name="key"/>,
    /// binary-safe: the key and value bytes are pinned as-is — no UTF-8
    /// round-trip — so arbitrary binary keys/values survive.
    /// <paramref name="ttlMs"/> 0 means no expiry.</summary>
    public void Set(ReadOnlySpan<byte> key, ReadOnlySpan<byte> value, long ttlMs = 0)
    {
        int rc;
        fixed (byte* kp = key)
        fixed (byte* vp = value.Length > 0 ? value : (ReadOnlySpan<byte>)NonEmpty)
        {
            rc = KevyNative.kevy_set(
                Live(), kp, (nuint)key.Length, vp, (nuint)value.Length,
                ttlMs > 0 ? (ulong)ttlMs : 0);
        }
        if (rc != 0) throw new KevyException("kevy: kevy_set misuse");
    }

    /// <summary>Set for a UTF-8 string key — a convenience wrapper over the
    /// binary-safe <see cref="Set(ReadOnlySpan{byte},ReadOnlySpan{byte},long)"/>.</summary>
    public void Set(string key, byte[] value, long ttlMs = 0) =>
        Set(Encoding.UTF8.GetBytes(key), value, ttlMs);

    /// <summary>Set for UTF-8 text values.</summary>
    public void Set(string key, string value, long ttlMs = 0) =>
        Set(key, Encoding.UTF8.GetBytes(value), ttlMs);

    /// <summary>Apply many SETs in one P/Invoke crossing — the batch-write
    /// path (kevy_set_many). A loop of <see cref="Set(string,byte[],long)"/>
    /// pays the boundary once per key; SetMany pays it once per ~1 MiB chunk,
    /// closing most of the bulk-write gap. keys and values must be equal
    /// length; durability is unchanged (each set appends to the AOF).</summary>
    public unsafe void SetMany(IReadOnlyList<byte[]> keys, IReadOnlyList<byte[]> values)
    {
        if (keys.Count != values.Count)
            throw new ArgumentException("kevy: SetMany keys/values length mismatch");
        int n = keys.Count;
        if (n == 0) return;
        const int arenaCap = 1 << 20; // ~1 MiB per crossing; memory stays bounded
        byte* arena = (byte*)NativeMemory.Alloc(arenaCap);
        byte** kp = (byte**)NativeMemory.Alloc((nuint)n, (nuint)sizeof(byte*));
        nuint* kl = (nuint*)NativeMemory.Alloc((nuint)n, (nuint)sizeof(nuint));
        byte** vp = (byte**)NativeMemory.Alloc((nuint)n, (nuint)sizeof(byte*));
        nuint* vl = (nuint*)NativeMemory.Alloc((nuint)n, (nuint)sizeof(nuint));
        try
        {
            int off = 0, cnt = 0;
            void Flush()
            {
                if (cnt == 0) return;
                if (KevyNative.kevy_set_many(Live(), (nuint)cnt, kp, kl, vp, vl) < 0)
                    throw new KevyException("kevy: kevy_set_many misuse");
                off = 0;
                cnt = 0;
            }
            for (int i = 0; i < n; i++)
            {
                int need = keys[i].Length + values[i].Length;
                if (need > arenaCap)
                {
                    Flush();
                    Set(keys[i], values[i]); // oversized: direct, no arena copy
                    continue;
                }
                if (cnt > 0 && off + need > arenaCap) { Flush(); i--; continue; }
                keys[i].CopyTo(new Span<byte>(arena + off, keys[i].Length));
                kp[cnt] = arena + off;
                kl[cnt] = (nuint)keys[i].Length;
                off += keys[i].Length;
                values[i].CopyTo(new Span<byte>(arena + off, values[i].Length));
                vp[cnt] = arena + off;
                vl[cnt] = (nuint)values[i].Length;
                off += values[i].Length;
                cnt++;
            }
            Flush();
        }
        finally
        {
            NativeMemory.Free(arena);
            NativeMemory.Free(kp);
            NativeMemory.Free(kl);
            NativeMemory.Free(vp);
            NativeMemory.Free(vl);
        }
    }

    /// <summary>Fetch <paramref name="key"/>'s raw bytes; null on a miss.
    /// Binary-safe: the key bytes are pinned as-is (no UTF-8 round-trip), so
    /// arbitrary binary keys work. Uses the zero-copy shared lane
    /// (kevy_get_shared): a bulk value comes back as an Arc clone — a refcount
    /// bump, no engine-side byte copy — and the one managed copy is made by
    /// <see cref="KevyNative.TakeBufShared"/>, which then drops the Arc via
    /// kevy_buf_free_shared. Throws on a non-string key (WRONGTYPE): the shared
    /// lane can't convey the typed error, so the unified client falls back to
    /// the framed GET for it.</summary>
    public byte[]? Get(ReadOnlySpan<byte> key)
    {
        int rc;
        KevyBuf @out;
        fixed (byte* kp = key)
        {
            rc = KevyNative.kevy_get_shared(Live(), kp, (nuint)key.Length, &@out);
        }
        return rc switch
        {
            1 => KevyNative.TakeBufShared(in @out),
            0 => null,
            _ => throw new KevyException("kevy: kevy_get_shared misuse"),
        };
    }

    /// <summary>Get for a UTF-8 string key — a convenience wrapper over the
    /// binary-safe <see cref="Get(ReadOnlySpan{byte})"/>.</summary>
    public byte[]? Get(string key) => Get(Encoding.UTF8.GetBytes(key));

    /// <summary>Reader for <see cref="GetView"/> — receives a span that VIEWS
    /// the value bytes in place. The span is valid ONLY for the call; do not
    /// stash it.</summary>
    public delegate void SpanReader(ReadOnlySpan<byte> value);

    /// <summary>Zero-copy read: hand <paramref name="reader"/> a span that
    /// VIEWs the value's bytes directly (an Arc refcount bump, no copy into
    /// managed memory), then release the view when it returns. This is the
    /// lane that beats a memory-mapped store on large reads — the shared lane
    /// is O(1) regardless of value size, where an mmap Get still copies out.
    /// Returns false on a miss (<paramref name="reader"/> is not called).
    ///
    /// The span is valid ONLY for the duration of the call — it aliases engine
    /// memory freed on return. Copy it out (<c>.ToArray()</c>) if you need to
    /// keep it. Throws on a non-string key (WRONGTYPE), like <see cref="Get(ReadOnlySpan{byte})"/>.</summary>
    public bool GetView(ReadOnlySpan<byte> key, SpanReader reader)
    {
        int rc;
        KevyBuf @out;
        fixed (byte* kp = key)
        {
            rc = KevyNative.kevy_get_shared(Live(), kp, (nuint)key.Length, &@out);
        }
        if (rc == 0) return false;
        if (rc < 0) throw new KevyException("kevy: kevy_get_shared misuse");
        try
        {
            reader(new ReadOnlySpan<byte>(@out.Ptr, checked((int)@out.Len)));
        }
        finally
        {
            KevyNative.kevy_buf_free_shared(@out.Ptr, @out.Len, @out.Cap);
        }
        return true;
    }

    /// <summary>Fetch <paramref name="key"/> as UTF-8 text; null on a miss.</summary>
    public string? GetText(string key)
    {
        var v = Get(key);
        return v is null ? null : Encoding.UTF8.GetString(v);
    }

    // ── the typed surface (mirrors the other kevy packages) ─────────────

    /// <summary>Remove keys, returning how many existed.</summary>
    public long Del(params string[] keys) => Want(Cmd(["DEL", .. keys])).AsLong ?? 0;

    /// <summary>Add <paramref name="delta"/> to the integer at
    /// <paramref name="key"/>, returning the result.</summary>
    public long IncrBy(string key, long delta = 1) =>
        Want(Cmd("INCRBY", key, delta.ToString())).AsLong ?? 0;

    /// <summary>Set <paramref name="key"/>'s time-to-live; false when the
    /// key does not exist.</summary>
    public bool Expire(string key, long ttlMs) =>
        Want(Cmd("PEXPIRE", key, ttlMs.ToString())).AsLong == 1;

    /// <summary>Remaining life in ms; Redis semantics (-1 no TTL, -2 no key).</summary>
    public long PttlMs(string key) => Want(Cmd("PTTL", key)).AsLong ?? -2;

    /// <summary>Keys matching a glob pattern. Page with Cmd("SCAN", …) on a
    /// large keyspace.</summary>
    public List<string> Keys(string pattern = "*")
    {
        if (Want(Cmd("KEYS", pattern)) is not KevyValue.Array a) return [];
        return [.. a.Items.Select(i => i.AsText).OfType<string>()];
    }

    /// <summary>Values for <paramref name="keys"/>, null per missing key.</summary>
    public List<byte[]?> Mget(params string[] keys)
    {
        if (Want(Cmd(["MGET", .. keys])) is not KevyValue.Array a) return [];
        return [.. a.Items.Select(i => (i as KevyValue.Bulk)?.Bytes)];
    }

    /// <summary>Live key count.</summary>
    public long DbSize() => Want(Cmd("DBSIZE")).AsLong ?? 0;

    /// <summary>Empty the store.</summary>
    public void FlushAll() => Want(Cmd("FLUSHALL"));

    /// <summary>The boot-replay verdict: what this open restored — and what
    /// it could not. <see cref="KevyOpenStats.DroppedBytes"/> &gt; 0 or
    /// <see cref="KevyOpenStats.Corrupt"/> means the store recovered LESS
    /// than its files held (the dropped region was quarantined next to the
    /// AOF): surface it as a startup health check instead of scraping the
    /// boot WARN line from stderr.</summary>
    public KevyOpenStats OpenReport()
    {
        KevyOpenReport r;
        if (KevyNative.kevy_open_report(Live(), &r) != 0)
            throw new KevyException("kevy: kevy_open_report misuse");
        return new KevyOpenStats
        {
            ReplayedCommands = r.ReplayedCommands,
            ReplayedBytes = r.ReplayedBytes,
            ElapsedMs = r.ElapsedMs,
            DroppedBytes = r.DroppedBytes,
            Corrupt = r.Corrupt != 0,
            QuarantineCount = r.QuarantineCount,
        };
    }

    /// <summary>Deliver <paramref name="payload"/> to every in-process
    /// subscriber on <paramref name="channel"/>; returns the receiver count.</summary>
    public long Publish(string channel, byte[] payload) =>
        Want(CmdBytes([Encoding.UTF8.GetBytes("PUBLISH"), Encoding.UTF8.GetBytes(channel), payload]))
            .AsLong ?? 0;

    /// <summary>Publish for UTF-8 text payloads.</summary>
    public long Publish(string channel, string payload) =>
        Publish(channel, Encoding.UTF8.GetBytes(payload));

    // ── pub/sub: callback registration + poll pump ──────────────────────

    /// <summary>Register <paramref name="callback"/> for
    /// <paramref name="channel"/> (a glob when <paramref name="pattern"/>).
    /// Frames are delivered by <see cref="Poll"/>, on whichever thread
    /// calls it.</summary>
    public KevySubscription Subscribe(
        string channel, Action<byte[], string> callback, bool pattern = false)
    {
        var c = Encoding.UTF8.GetBytes(channel);
        IntPtr h;
        fixed (byte* cp = c)
        {
            h = pattern
                ? KevyNative.kevy_psubscribe(Live(), cp, (nuint)c.Length)
                : KevyNative.kevy_subscribe(Live(), cp, (nuint)c.Length);
        }
        if (h == IntPtr.Zero) throw new KevyException($"kevy: subscribe failed: {channel}");
        var sub = new KevySubscription(h, callback);
        _subs.Add(sub);
        return sub;
    }

    /// <summary>Drain every subscription queue, dispatching callbacks.
    /// Returns the number of frames drained. Call from your loop/timer.</summary>
    public int Poll()
    {
        var n = 0;
        foreach (var s in _subs) n += s.Poll();
        _subs.RemoveAll(s => s.IsClosed);
        return n;
    }

    /// <summary>Flush every shard's AOF with a REAL fsync, write the feed
    /// continuity marker, then refuse every later write (reads stay
    /// available) — the deterministic teardown for a host's signal handler:
    /// Shutdown(), then exit. Idempotent. Throws <see cref="KevyException"/>
    /// on an I/O failure (the store is still usable — retry or exit).</summary>
    public void Shutdown()
    {
        var rc = KevyNative.kevy_shutdown(Live());
        if (rc == 0) return;
        throw new KevyException(rc == -2
            ? "kevy: shutdown flush failed (fsync/marker I/O error; store still usable — retry or exit)"
            : "kevy: kevy_shutdown misuse");
    }

    /// <summary>Close the store. Safe to call more than once.</summary>
    public void Dispose()
    {
        foreach (var s in _subs) s.Dispose();
        _subs.Clear();
        if (_db != IntPtr.Zero)
        {
            KevyNative.kevy_close(_db);
            _db = IntPtr.Zero;
        }
    }

    private static KevyValue Want(KevyValue v) =>
        v is KevyValue.Error e ? throw new KevyException($"kevy: {e.Message}") : v;
}
