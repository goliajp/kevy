// RESP2/RESP3 decode + CRC16 (contract §4.1, §3.15).

using System.Text;
using Kevy;
using Xunit;

namespace Kevy.Tests;

public class ReplyTests
{
    private static Reply Dec(string wire) => Reply.Decode(Encoding.ASCII.GetBytes(wire));

    [Fact]
    public void SimpleAndError()
    {
        Assert.Equal(ReplyKind.Simple, Dec("+OK\r\n").Kind);
        Assert.Equal("OK", Dec("+OK\r\n").Str());
        var e = Dec("-WRONGTYPE nope\r\n");
        Assert.Equal(ReplyKind.Error, e.Kind);
        Assert.Equal("WRONGTYPE nope", e.Str());
    }

    [Fact]
    public void IntBulkNil()
    {
        Assert.Equal(42, Dec(":42\r\n").Integer);
        Assert.Equal("hi", Dec("$2\r\nhi\r\n").Str());
        Assert.True(Dec("$-1\r\n").IsNil);
        Assert.True(Dec("*-1\r\n").IsNil);
    }

    [Fact]
    public void ArrayNested()
    {
        var a = Dec("*2\r\n:1\r\n$3\r\nfoo\r\n");
        Assert.Equal(ReplyKind.Array, a.Kind);
        Assert.Equal(2, a.Items.Count);
        Assert.Equal(1, a.Items[0].Integer);
        Assert.Equal("foo", a.Items[1].Str());
    }

    [Fact]
    public void Resp3Shapes()
    {
        Assert.Equal(3.14, Dec(",3.14\r\n").Double, 3);
        Assert.True(Dec("#t\r\n").Bool);
        Assert.False(Dec("#f\r\n").Bool);
        Assert.True(Dec("_\r\n").IsNil);
        Assert.Equal(ReplyKind.Map, Dec("%1\r\n+k\r\n:9\r\n").Kind);
        Assert.Equal(ReplyKind.Set, Dec("~2\r\n:1\r\n:2\r\n").Kind);
        Assert.Equal(ReplyKind.Push, Dec(">3\r\n$7\r\nmessage\r\n$2\r\nch\r\n$2\r\nhi\r\n").Kind);
        var vb = Dec("=9\r\ntxt:hello\r\n");
        Assert.Equal(ReplyKind.Verbatim, vb.Kind);
        Assert.Equal("hello", vb.Str());
    }

    [Fact]
    public void AttributesAreSkipped()
    {
        // |1 attr map decorating :7 → the :7 survives, attr discarded.
        var r = Dec("|1\r\n+ttl\r\n:1\r\n:7\r\n");
        Assert.Equal(ReplyKind.Int, r.Kind);
        Assert.Equal(7, r.Integer);
    }

    [Fact]
    public void Crc16CheckVector() =>
        Assert.Equal(0x31C3, Crc16.Hash(Encoding.ASCII.GetBytes("123456789")));

    [Fact]
    public void HashTagRouting()
    {
        // {user1000} extracts to the same slot regardless of surrounding key.
        Assert.Equal(
            Crc16.KeyHashSlot(Encoding.ASCII.GetBytes("{user1000}.following")),
            Crc16.KeyHashSlot(Encoding.ASCII.GetBytes("{user1000}.followers")));
    }
}
