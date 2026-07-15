// Error-as-exception mapping on both backends (contract §6 "Error-as-value
// / exception mapping", §2).

using Kevy;
using Xunit;

namespace Kevy.Tests;

[Collection("server")]
public class ErrorsTests(ServerFixture fx)
{
    [Fact]
    public void WrongTypeIsStoreError() => H.Both(fx, c =>
    {
        c.Set("k", "v");
        var e = Assert.Throws<KevyStoreException>(() => c.LPush("k", "x"));
        Assert.Equal(StoreError.WrongType, e.Error);
    });

    [Fact]
    public void IncrNonNumericIsStoreError() => H.Both(fx, c =>
    {
        c.Set("k", "notanumber");
        var e = Assert.Throws<KevyStoreException>(() => c.Incr("k"));
        Assert.Equal(StoreError.NotInteger, e.Error);
    });

    [Fact]
    public void RawErrorIsInlineValueNotThrown() => H.Both(fx, c =>
    {
        // A -ERR from the raw escape hatch is DATA (Reply.Error), not an
        // exception — matching the pipeline inline-error convention.
        var r = c.Do("GET"); // wrong arity
        Assert.True(r.IsError);
    });

    [Fact]
    public void WireTextPreservedVerbatim() => H.Both(fx, c =>
    {
        var r = c.Do("GET"); // "-ERR wrong number of arguments…"
        Assert.Contains("wrong number of arguments", r.Str());
    });
}
