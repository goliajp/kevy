// Connection & URL routing (contract §6, §1.1–§1.3).

using Kevy;
using Xunit;

namespace Kevy.Tests;

public class UrlTests
{
    [Fact]
    public void RedissAndKevysAreUnsupported()
    {
        Assert.Throws<KevyUnsupportedException>(() => KevyClient.Connect("rediss://h:6379"));
        Assert.Throws<KevyUnsupportedException>(() => KevyClient.Connect("kevys://h:6379"));
    }

    [Fact]
    public void UserinfoIsUnsupported() =>
        Assert.Throws<KevyUnsupportedException>(() => KevyClient.Connect("redis://user:pass@h:6379"));

    [Fact]
    public void UnknownSchemeIsInvalidInput() =>
        Assert.Throws<KevyInvalidInputException>(() => KevyClient.Connect("weird://h"));

    [Fact]
    public void EmptyFilePathIsInvalidInput() =>
        Assert.Throws<KevyInvalidInputException>(() => KevyClient.Connect("file://"));

    [Fact]
    public void MissingSchemeSeparatorIsInvalidInput() =>
        Assert.Throws<KevyInvalidInputException>(() => KevyClient.Connect("localhost:6379"));

    [Fact]
    public void AnonymousMemIsIsolated()
    {
        using var a = KevyClient.Connect("mem://");
        using var b = KevyClient.Connect("mem://");
        a.Set("k", "va");
        b.Set("k", "vb");
        Assert.Equal("va", H.Str(a.Get("k")));
        Assert.Equal("vb", H.Str(b.Get("k")));
    }

    [Fact]
    public void NamedMemSharesOneStore()
    {
        var url = H.Mem();
        using var a = KevyClient.Connect(url);
        using var b = KevyClient.Connect(url);
        a.Set("shared", "hello");
        Assert.Equal("hello", H.Str(b.Get("shared")));
    }

    [Fact]
    public void FileUrlSharesAndPersists()
    {
        var dir = Directory.CreateTempSubdirectory("kevy-file-").FullName;
        var url = "file://" + dir;
        using (var a = KevyClient.Connect(url))
        using (var b = KevyClient.Connect(url))
        {
            a.Set("p", "v1");
            Assert.Equal("v1", H.Str(b.Get("p")));
        }
        // Reopen after both handles dropped → snapshot + AOF replay.
        using var c = KevyClient.Connect(url);
        Assert.Equal("v1", H.Str(c.Get("p")));
    }

    [Fact]
    public void EmbeddedFlag()
    {
        using var emb = KevyClient.Connect("mem://");
        Assert.True(emb.IsEmbedded);
    }
}
