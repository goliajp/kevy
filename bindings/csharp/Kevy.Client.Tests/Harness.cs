// Test harness (contract §6): locate + spawn a real kevy server for the
// remote-backend tests, point the embedded FFI at the freshly-built engine
// cdylib, and provide a "both backends" helper so each conformance
// behaviour runs against embedded (mem://) and remote (kevy://) alike.

using System.Diagnostics;
using System.Net;
using System.Net.Sockets;
using System.Runtime.CompilerServices;
using Kevy;
using Xunit;

namespace Kevy.Tests;

internal static class Env
{
    // Point the embedded P/Invoke door at the just-built engine cdylib
    // before any FFI call (the KevyNative resolver reads KEVY_FFI_LIB).
    [ModuleInitializer]
    internal static void Init()
    {
        if (System.Environment.GetEnvironmentVariable("KEVY_FFI_LIB") is not null) return;
        var lib = FindFfiLib();
        if (lib is not null) System.Environment.SetEnvironmentVariable("KEVY_FFI_LIB", lib);
    }

    internal static string RepoRoot()
    {
        var d = new DirectoryInfo(AppContext.BaseDirectory);
        while (d is not null)
        {
            if (Directory.Exists(Path.Combine(d.FullName, "crates")) &&
                File.Exists(Path.Combine(d.FullName, "Cargo.toml")))
                return d.FullName;
            d = d.Parent;
        }
        return "/Users/doracawl/workspace/goliajp/kevy";
    }

    // A git worktree has its own tree but shares the main repo's target/
    // build output, so probe both roots (the worktree first).
    private static IEnumerable<string> Roots()
    {
        yield return RepoRoot();
        yield return "/Users/doracawl/workspace/goliajp/kevy";
    }

    private static string? Locate(params string[] rels)
    {
        foreach (var root in Roots())
            foreach (var rel in rels)
            {
                var p = Path.Combine(root, rel);
                if (File.Exists(p)) return p;
            }
        return null;
    }

    private static string? FindFfiLib() =>
        Locate("target/release/libkevy_ffi.dylib", "target/debug/libkevy_ffi.dylib",
               "bindings/csharp/Kevy.Embedded/runtimes/osx-arm64/native/libkevy_ffi.dylib");

    internal static string? ServerBinary() =>
        Locate("target/release/kevy", "target/debug/kevy");
}

/// <summary>A real kevy server spawned once per test session (shared by the
/// remote-backend tests; each test flushes for a clean slate).</summary>
public sealed class ServerFixture : IDisposable
{
    // Not readonly: the port-collision retry in the constructor
    // disposes a loser and respawns via SpawnProcess.
    private Process? _proc;
    private readonly string _tmp;
    /// <summary>The server's stderr, drained as it arrives. A spawn that
    /// dies during startup otherwise reports only "exited", which is the
    /// one thing already known.</summary>
    private readonly System.Text.StringBuilder _stderr = new();
    public string? Url { get; }
    public int Port { get; }

    public ServerFixture() : this(Array.Empty<string>(), null) { }

    /// <summary>Spawn a server with extra args / a TOML config (used by
    /// feed + cluster tests). The caller disposes it.</summary>
    public static ServerFixture Spawn(string[] extra, string? config = null) => new(extra, config);

    private ServerFixture(string[] extra, string? config)
    {
        var bin = Env.ServerBinary();
        _tmp = Directory.CreateTempSubdirectory("kevy-xunit-").FullName;
        if (bin is null) return;
        // FreePort's probe→bind window can hand two nearby spawns the
        // same port; the loser exits at bind with "address already in
        // use" on stderr. Retry that exact signature on a fresh port;
        // any other startup death rethrows unchanged.
        for (var attempt = 1; ; attempt++)
        {
            var port = FreePort();
            Port = port;
            Url = $"kevy://127.0.0.1:{port}";
            SpawnProcess(bin, port, extra, config);
            try { WaitReady(); return; }
            catch (InvalidOperationException e)
                when (attempt < 5 && e.Message.Contains("already in use", StringComparison.OrdinalIgnoreCase))
            {
                _proc?.Dispose();
                _proc = null;
                lock (_stderr) _stderr.Clear();
            }
        }
    }

    private void SpawnProcess(string bin, int port, string[] extra, string? config)
    {
        var args = new List<string> { "--bind", "127.0.0.1", "--port", port.ToString(), "--dir", _tmp };
        if (config is not null)
        {
            var cfg = Path.Combine(_tmp, "kevy.toml");
            File.WriteAllText(cfg, config);
            args.Add("--config"); args.Add(cfg);
        }
        args.AddRange(extra);
        _proc = Process.Start(new ProcessStartInfo(bin) { RedirectStandardOutput = false, RedirectStandardError = true }
            .With(args));
        if (_proc is not null)
        {
            // Drained asynchronously: a pipe nobody reads fills up and
            // blocks the very process being waited on.
            _proc.ErrorDataReceived += (_, e) => { if (e.Data is not null) lock (_stderr) _stderr.AppendLine(e.Data); };
            _proc.BeginErrorReadLine();
        }
    }

    /// <summary>The remote URL. Callers guard on <see cref="HasServer"/>
    /// (via Assert.SkipUnless) before using it.</summary>
    public string RequireUrl() => Url ?? throw new InvalidOperationException("no server");

    public bool HasServer => Url is not null;

    private void WaitReady()
    {
        var deadline = DateTime.UtcNow.AddSeconds(10);
        while (DateTime.UtcNow < deadline)
        {
            if (_proc is { HasExited: true })
            {
                string why;
                lock (_stderr) why = _stderr.ToString().Trim();
                throw new InvalidOperationException(
                    $"server exited before ready (port {Port}, exit {_proc.ExitCode})"
                    + (why.Length == 0 ? " with no output" : $": {why}"));
            }
            try
            {
                using var c = KevyClient.Connect(Url!);
                c.Ping();
                return;
            }
            catch (KevyException) { Thread.Sleep(30); }
        }
        throw new InvalidOperationException("server never became ready");
    }

    /// <summary>A port the OS says is free.
    ///
    /// There is a window between this listener closing and the server
    /// binding, so two spawns close together can be handed the same port
    /// and the loser exits immediately — the diagnosis the captured
    /// stderr confirmed (same recipe as the Rust harness's spop_storm
    /// fix). The constructor retries that signature on a fresh port.</summary>
    internal static int FreePort()
    {
        var l = new TcpListener(IPAddress.Loopback, 0);
        l.Start();
        var port = ((IPEndPoint)l.LocalEndpoint).Port;
        l.Stop();
        return port;
    }

    public void Dispose()
    {
        try { if (_proc is { HasExited: false }) { _proc.Kill(true); _proc.WaitForExit(2000); } } catch { }
        try { Directory.Delete(_tmp, true); } catch { }
    }
}

internal static class ProcExt
{
    internal static ProcessStartInfo With(this ProcessStartInfo psi, IEnumerable<string> args)
    {
        foreach (var a in args) psi.ArgumentList.Add(a);
        return psi;
    }
}

[CollectionDefinition("server")]
public sealed class ServerCollection : ICollectionFixture<ServerFixture> { }

/// <summary>Shared both-backends helpers.</summary>
internal static class H
{
    private static int _n;

    internal static string Mem() => $"mem://t-{Interlocked.Increment(ref _n)}";

    // Run the same assertions against embedded and (if available) remote.
    internal static void Both(ServerFixture fx, Action<KevyClient> body)
    {
        using (var emb = KevyClient.Connect(Mem())) body(emb);
        if (!fx.HasServer) return;
        using var rem = KevyClient.Connect(fx.RequireUrl());
        rem.FlushAll();
        body(rem);
    }

    internal static async Task BothAsync(ServerFixture fx, Func<KevyClient, Task> body)
    {
        using (var emb = await KevyClient.ConnectAsync(Mem())) await body(emb);
        if (!fx.HasServer) return;
        using var rem = await KevyClient.ConnectAsync(fx.RequireUrl());
        await rem.FlushAllAsync();
        await body(rem);
    }

    // The URLs to exercise pub/sub over: a named embedded bus + (if
    // available) the remote server. Named/remote so a publisher and a
    // subscriber on the same URL find each other.
    internal static IEnumerable<string> PubsubUrls(ServerFixture fx)
    {
        yield return Mem();
        if (fx.HasServer) yield return fx.RequireUrl();
    }

    internal static bool IsRemote(string url) => url.StartsWith("kevy") || url.StartsWith("redis") || url.StartsWith("tcp");

    internal static byte[] B(string s) => System.Text.Encoding.UTF8.GetBytes(s);

    internal static string Str(byte[]? b) => b is null ? "<nil>" : System.Text.Encoding.UTF8.GetString(b);
}
