// The C# smoke: what ffigate runs on every push. Opens a persistent
// store, exercises the command entry and the typed surface, round-trips
// pub/sub, then reopens the same directory and proves the data survived.
// Exits non-zero on the first lie.
//
//   KEVY_FFI_LIB=target/debug/libkevy_ffi.dylib \
//     dotnet run --project bindings/csharp/smoke -- /tmp/kevy-smoke-cs

using System.Text;
using Kevy.Embedded;

if (args.Length != 1)
{
    Console.Error.WriteLine("usage: smoke <data-dir>");
    return 2;
}

Check(KevyDb.Abi() == 1, "abi");
Console.WriteLine($"kevy {KevyDb.Version()} / abi {KevyDb.Abi()}");

var db = KevyDb.Open(args[0]);

// The command entry, RESP semantics.
Check(db.Cmd("SET", "smoke:k", "v1") is KevyValue.Simple { Value: "OK" }, "SET");
Check(db.Cmd("GET", "smoke:k").AsText == "v1", "GET");

// A protocol error is DATA from Cmd — and a THROW from the typed layer.
Check(db.Cmd("NOSUCHVERB").IsError, "error as data");
Check(Throws(() => db.IncrBy("smoke:k")), "typed WRONGTYPE throws");

// The typed surface.
db.Set("smoke:t", "x", ttlMs: 30_000);
var ttl = db.PttlMs("smoke:t");
Check(ttl is > 0 and <= 30_000, "PTTL");
Check(db.IncrBy("smoke:hits", 5) == 5, "INCRBY");
Check(db.Mget("smoke:k", "smoke:none")[1] is null, "MGET miss");
Check(db.Keys("smoke:*").Count == 3, "KEYS");
Check(db.DbSize() == 3, "DBSIZE");

// Pub/sub round trip through the poll pump.
var seen = new List<(string Channel, string Payload)>();
db.Subscribe("c1", (payload, chan) => seen.Add((chan, Encoding.UTF8.GetString(payload))));
Check(db.Publish("c1", "hello") == 1, "PUBLISH");
db.Poll();
Check(seen is [("c1", "hello")], "pubsub frame");

// The scalar fast path.
db.Set("fast:k", "fv");
Check(db.GetText("fast:k") == "fv", "fast get");
Check(db.Get("fast:none") is null, "fast miss");

// Durability: close, reopen the same dir, the key is still there.
db.Dispose();
db = KevyDb.Open(args[0]);
Check(db.GetText("smoke:k") == "v1", "GET after reopen");
Check(db.Del("smoke:k", "smoke:none") == 1, "DEL");
db.Dispose();

Console.WriteLine("smoke: ok");
return 0;

static void Check(bool ok, string what)
{
    if (ok) return;
    Console.Error.WriteLine($"FAIL {what}");
    Environment.Exit(1);
}

static bool Throws(Action a)
{
    try
    {
        a();
        return false;
    }
    catch (KevyException)
    {
        return true;
    }
}
