// clientgate: StackExchange.Redis against a kevy server.
using StackExchange.Redis;

var port = Environment.GetEnvironmentVariable("KEVY_PORT");
var mux = await ConnectionMultiplexer.ConnectAsync($"127.0.0.1:{port},allowAdmin=true");
var db = mux.GetDatabase();

void Check(bool ok, string what)
{
    if (ok) return;
    Console.Error.WriteLine($"FAIL {what}");
    Environment.Exit(1);
}

await mux.GetServer($"127.0.0.1:{port}").FlushAllDatabasesAsync();
Check(await db.StringSetAsync("k", "v"), "SET");
Check((await db.StringGetAsync("k")) == "v", "GET");
Check(await db.StringIncrementAsync("n", 5) == 5, "INCRBY");
Check(await db.KeyExpireAsync("k", TimeSpan.FromSeconds(30)), "PEXPIRE");
var ttl = await db.KeyTimeToLiveAsync("k");
Check(ttl is { TotalMilliseconds: > 0 and <= 30_000 }, "PTTL");

await db.HashSetAsync("h", [new HashEntry("a", "1"), new HashEntry("b", "2")]);
Check((await db.HashGetAsync("h", "a")) == "1", "HGETALL");
await db.ListLeftPushAsync("l", ["x", "y"]);
var lr = await db.ListRangeAsync("l");
Check(lr.Length == 2 && lr[0] == "y" && lr[1] == "x", "LRANGE");
await db.SortedSetAddAsync("z", [new SortedSetEntry("one", 1), new SortedSetEntry("two", 2)]);
var zr = await db.SortedSetRangeByRankAsync("z");
Check(zr.Length == 2 && zr[0] == "one" && zr[1] == "two", "ZRANGE");

// pub/sub round trip
var got = new TaskCompletionSource<string>();
var subch = RedisChannel.Literal("room");
await mux.GetSubscriber().SubscribeAsync(subch, (_, msg) => got.TrySetResult(msg!));
await db.PublishAsync(subch, "hi");
Check(await got.Task.WaitAsync(TimeSpan.FromSeconds(5)) == "hi", "pubsub");

// the extended verb surface through the raw channel
var idx = await db.ExecuteAsync("IDX.LIST");
Check(idx.Resp2Type == ResultType.Array, "IDX.LIST raw");

Console.WriteLine("ok");
