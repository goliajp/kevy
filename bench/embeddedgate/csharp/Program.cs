// embeddedgate — C# track. kevy C# scalar (KevyDb.Get/Set, returns byte[])
// vs LMDB via LightningDB. Both return owned managed arrays on read, so this
// is copy-vs-copy through .NET bindings — it isolates the managed-binding tax
// from the engine (the C track measured LMDB direct). Headline tier T-async:
// kevy AOF EverySec, LMDB EnvironmentOpenFlags.NoSync. LMDB forces a txn;
// cold-1op = txn/op, amortized = one txn/N. k/p < 1 means kevy faster.
//
// Relative standing from the dev host; definitive SLA = lx64 (perf §9).
using System.Diagnostics;
using Kevy.Embedded;
using LightningDB;

const int N = 100_000, KEYS = 200, RUNS = 3;
int[] sizes = { 16, 256, 4096, 65536 };

var keys = new byte[KEYS][];
var keyStr = new string[KEYS];
for (int i = 0; i < KEYS; i++) { keyStr[i] = $"k:{i}"; keys[i] = System.Text.Encoding.UTF8.GetBytes(keyStr[i]); }

double Timeit(Action fn)
{
    var s = new double[RUNS];
    for (int r = 0; r < RUNS; r++)
    {
        long t0 = Stopwatch.GetTimestamp();
        fn();
        long t1 = Stopwatch.GetTimestamp();
        s[r] = (t1 - t0) * 1e9 / Stopwatch.Frequency / N;
    }
    Array.Sort(s);
    return s[1];
}

(double get, double getView, double setCold, double setAmort) BenchKevy(KevyDb db, byte[] v)
{
    for (int i = 0; i < KEYS; i++) db.Set(keys[i], v);
    double get = Timeit(() => { for (int i = 0; i < N; i++) _ = db.Get(keys[i % KEYS]); });
    // zero-copy read: view the value span in place, no managed copy
    KevyDb.SpanReader noop = _ => { };
    double getView = Timeit(() => { for (int i = 0; i < N; i++) db.GetView(keys[i % KEYS], noop); });
    double setCold = Timeit(() => { for (int i = 0; i < N; i++) db.Set(keys[i % KEYS], v); });
    // amortized: SetMany — one P/Invoke crossing per ~1 MiB chunk (batch path)
    var mk = new byte[N][];
    var mv = new byte[N][];
    for (int i = 0; i < N; i++) { mk[i] = keys[i % KEYS]; mv[i] = v; }
    double setAmort = Timeit(() => db.SetMany(mk, mv));
    return (get, getView, setCold, setAmort);
}

(double getCold, double getAmort, double setCold, double setAmort) BenchLmdb(LightningEnvironment env, byte[] v)
{
    LightningDatabase db;
    using (var tx = env.BeginTransaction())
    {
        db = tx.OpenDatabase(configuration: new DatabaseConfiguration { Flags = DatabaseOpenFlags.Create });
        for (int i = 0; i < KEYS; i++) tx.Put(db, keys[i], v);
        tx.Commit();
    }

    // GET cold: one rdonly txn per get, value copied out to a managed array
    double getCold = Timeit(() =>
    {
        for (int i = 0; i < N; i++)
        {
            using var tx = env.BeginTransaction(TransactionBeginFlags.ReadOnly);
            var (code, _, val) = tx.Get(db, keys[i % KEYS]);
            _ = val.CopyToNewArray();
        }
    });
    // GET amortized: one rdonly txn reused for all N gets
    double getAmort = Timeit(() =>
    {
        using var tx = env.BeginTransaction(TransactionBeginFlags.ReadOnly);
        for (int i = 0; i < N; i++)
        {
            var (code, _, val) = tx.Get(db, keys[i % KEYS]);
            _ = val.CopyToNewArray();
        }
    });
    // SET cold: begin+put+commit per op
    double setCold = Timeit(() =>
    {
        for (int i = 0; i < N; i++)
        {
            using var tx = env.BeginTransaction();
            tx.Put(db, keys[i % KEYS], v);
            tx.Commit();
        }
    });
    // SET amortized: one txn wrapping N puts
    double setAmort = Timeit(() =>
    {
        using var tx = env.BeginTransaction();
        for (int i = 0; i < N; i++) tx.Put(db, keys[i % KEYS], v);
        tx.Commit();
    });
    return (getCold, getAmort, setCold, setAmort);
}

string Verdict(double k, double p)
{
    double r = k / p;
    return r < 0.97 ? $"kevy {p / k:F2}x" : r > 1.03 ? $"peer {k / p:F2}x" : "tie";
}
void Table(string field, double[] kevy, double[] peer)
{
    Console.WriteLine($"\n### {field}");
    Console.WriteLine("|   size | kevy ns | lmdb ns | k/p | verdict |");
    Console.WriteLine("|-------:|--------:|--------:|----:|---------|");
    for (int i = 0; i < sizes.Length; i++)
        Console.WriteLine($"| {sizes[i],6} | {kevy[i],7:F0} | {peer[i],7:F0} | {kevy[i] / peer[i],4:F2} | {Verdict(kevy[i], peer[i])} |");
}

var root = Directory.CreateTempSubdirectory("embgate-cs-").FullName;

// SetMany correctness gate before timing: batch a mix incl. an empty key and a
// 2 MiB value (exercises the oversized-item direct fallback), read each back.
using (var vdb = KevyDb.OpenInMemory())
{
    // (empty keys are a separate C# Get limitation — fixed on an empty span is
    // a null ptr — unrelated to SetMany; not exercised here.)
    var vk = new byte[][] { "a"u8.ToArray(), "bb"u8.ToArray(), "ccc"u8.ToArray(), "big"u8.ToArray() };
    var vv = new byte[][] { "1"u8.ToArray(), new byte[4096], "z"u8.ToArray(), new byte[2 << 20] };
    Array.Fill(vv[1], (byte)0x61);
    Array.Fill(vv[3], (byte)0x62);
    vdb.SetMany(vk, vv);
    for (int i = 0; i < vk.Length; i++)
    {
        var got = vdb.Get(vk[i]) ?? throw new Exception($"SetMany: key {i} missing");
        if (got.Length != vv[i].Length) throw new Exception($"SetMany: key {i} len {got.Length} != {vv[i].Length}");
    }
    // GetView zero-copy: the viewed span must equal the stored value; a miss
    // must not call the reader.
    int viewLen = -1;
    bool hit = vdb.GetView(vk[1], span => viewLen = span.Length);
    if (!hit || viewLen != vv[1].Length) throw new Exception($"GetView: hit={hit} len={viewLen}");
    bool missCalled = false;
    if (vdb.GetView("nope"u8.ToArray(), _ => missCalled = true) || missCalled)
        throw new Exception("GetView(miss) should return false and not call reader");
    Console.WriteLine("setmany-verify: OK · getview-verify: OK");
}

Console.WriteLine("# embeddedgate — C# — kevy C# scalar vs LMDB (LightningDB)");
Console.WriteLine($"N={N} ops, {KEYS} warm keys, median-of-{RUNS}, sizes 16/256/4096/65536 B");
Console.WriteLine("kevy: cold==amortized (no txn). LMDB: cold=txn/op, amort=one txn/N. Both copy out to byte[].");
Console.WriteLine("\n## T-async — kevy AOF EverySec / LMDB NoSync (OS-flush, no per-op fsync)");

int NS = sizes.Length;
double[] kgc = new double[NS], kga = new double[NS], kgv = new double[NS], ksc = new double[NS], ksa = new double[NS];
double[] lgc = new double[NS], lga = new double[NS], lsc = new double[NS], lsa = new double[NS];

for (int i = 0; i < NS; i++)
{
    var v = new byte[sizes[i]];
    Array.Fill(v, (byte)0x61);

    var kdir = Path.Combine(root, $"kevy-{sizes[i]}");
    Directory.CreateDirectory(kdir);
    using (var kdb = KevyDb.Open(kdir))
    {
        var (g, gv, sc, sa) = BenchKevy(kdb, v);
        kgc[i] = g; kga[i] = g; kgv[i] = gv; ksc[i] = sc; ksa[i] = sa;
    }

    var ldir = Path.Combine(root, $"lmdb-{sizes[i]}");
    Directory.CreateDirectory(ldir);
    using (var env = new LightningEnvironment(ldir) { MapSize = 1L << 30, MaxDatabases = 1 })
    {
        env.Open(EnvironmentOpenFlags.NoSync);
        var (gc, ga, sc, sa) = BenchLmdb(env, v);
        lgc[i] = gc; lga[i] = ga; lsc[i] = sc; lsa[i] = sa;
    }
}

Table("GET cold-1op", kgc, lgc);
Table("GET amortized", kga, lga);
Table("GET zero-copy — kevy GetView vs LMDB copy-out", kgv, lga);
Table("SET cold-1op", ksc, lsc);
Table("SET amortized", ksa, lsa);
Console.WriteLine("\n(relative standing — dev host; definitive SLA = lx64 per perf §9)");
