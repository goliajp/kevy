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
    public void GetOnWrongTypeIsStoreError() => H.Both(fx, c =>
    {
        // GET on a list must surface WRONGTYPE, not collapse to a miss. On the
        // embedded backend the zero-copy scalar shared lane can't convey
        // WRONGTYPE, so the unified Get falls back to the framed GET to
        // preserve the typed error — matching the remote backend.
        c.LPush("lst", "a");
        var e = Assert.Throws<KevyStoreException>(() => c.Get("lst"));
        Assert.Equal(StoreError.WrongType, e.Error);
    });

    [Fact]
    public Task GetAsyncOnWrongTypeIsStoreError() => H.BothAsync(fx, async c =>
    {
        // Same fallback on the async face: the embedded scalar-lane Get
        // completes synchronously, then routes a wrong-type error through the
        // framed GET so the typed WRONGTYPE surfaces on both backends.
        await c.LPushAsync("lst", ["a"]);
        var e = await Assert.ThrowsAsync<KevyStoreException>(async () => await c.GetAsync("lst"));
        Assert.Equal(StoreError.WrongType, e.Error);
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
