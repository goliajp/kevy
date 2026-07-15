// Shapes a RESP2 array or RESP3 push frame into a PubsubEvent (contract
// §3.11: both delivery shapes accepted transparently). Ack frames
// (subscribe/unsubscribe) and deliveries (message/pmessage) alike.

using System.Text;

namespace Kevy;

internal static class PubsubFrame
{
    internal static PubsubEvent Classify(Reply r)
    {
        IReadOnlyList<Reply> items = r.Kind switch
        {
            ReplyKind.Array or ReplyKind.Push => r.Items,
            ReplyKind.Error => throw Err.ReplyError(r),
            _ => throw new KevyProtocolException("expected array/push frame, got " + r.Shape()),
        };
        if (items.Count == 0 || items[0].Kind != ReplyKind.Bulk)
            throw new KevyProtocolException("pubsub frame missing kind field");
        return Shape(Encoding.ASCII.GetString(items[0].Bytes), items);
    }

    private static PubsubEvent Shape(string kind, IReadOnlyList<Reply> items) => kind switch
    {
        "subscribe" => Ack(PubsubKind.Subscribe, items, false),
        "psubscribe" => Ack(PubsubKind.Psubscribe, items, true),
        "unsubscribe" => Ack(PubsubKind.Unsubscribe, items, false),
        "punsubscribe" => Ack(PubsubKind.Punsubscribe, items, true),
        "message" => Message(items),
        "pmessage" => Pmessage(items),
        _ => throw new KevyProtocolException($"unknown pubsub kind '{kind}'"),
    };

    private static PubsubEvent Message(IReadOnlyList<Reply> items)
    {
        if (items.Count != 3 || items[1].Kind != ReplyKind.Bulk || items[2].Kind != ReplyKind.Bulk)
            throw new KevyProtocolException("bad message frame");
        return new PubsubEvent(PubsubKind.Message, items[1].Bytes, null, items[2].Bytes, 0);
    }

    private static PubsubEvent Pmessage(IReadOnlyList<Reply> items)
    {
        if (items.Count != 4 || items[1].Kind != ReplyKind.Bulk ||
            items[2].Kind != ReplyKind.Bulk || items[3].Kind != ReplyKind.Bulk)
            throw new KevyProtocolException("bad pmessage frame");
        return new PubsubEvent(PubsubKind.Pmessage, items[2].Bytes, items[1].Bytes, items[3].Bytes, 0);
    }

    private static PubsubEvent Ack(PubsubKind kind, IReadOnlyList<Reply> items, bool pattern)
    {
        if (items.Count != 3) throw new KevyProtocolException($"expected 3-element pubsub frame, got {items.Count}");
        byte[]? name = items[1].Kind switch
        {
            ReplyKind.Bulk => items[1].Bytes,
            ReplyKind.Nil or ReplyKind.Null => null,
            _ => throw new KevyProtocolException("bad pubsub name field"),
        };
        if (items[2].Kind != ReplyKind.Int) throw new KevyProtocolException("bad pubsub count field");
        var count = items[2].Integer;
        return pattern
            ? new PubsubEvent(kind, null, name, null, count)
            : new PubsubEvent(kind, name, null, null, count);
    }
}
