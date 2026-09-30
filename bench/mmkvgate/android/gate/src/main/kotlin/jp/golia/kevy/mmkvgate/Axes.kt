// The axes, as on Apple. Warm = a small reused keyspace; cold set = every
// key new; reopened get = first read of each key after the store is
// reopened; batch = one call carrying many keys where the engine has one
// (kevy MSET / MGET; MMKV has no batch API, so its batch is the loop an
// app would write); open = open a store holding 10k keys and read one;
// close = close that store, whatever it waits for.
package jp.golia.kevy.mmkvgate

import java.io.File

private const val OPS = 2000
private const val KEYS = 200
private val SIZES = intArrayOf(16, 256, 4096)

class Axes(private val root: File, private val out: MutableList<String>) {
    fun all() {
        warmGet(); warmSet(); coldSet(); reopenedGet(); batch(); startup()
    }

    fun warmGet() {
        for (size in SIZES) {
            val v = payload(size)
            val k = KevyEngine(root, "get"); val m = MmkvEngine("get")
            for (i in 0 until KEYS) { k.set("k$i", v); m.set("k$i", v) }
            val keys = List(OPS) { "k${it % KEYS}" }
            cell(out, "get_warm", size, OPS,
                { timeNs { for (key in keys) check(k.get(key)?.size == size) } },
                { timeNs { for (key in keys) check(m.get(key)?.size == size) } })
        }
    }

    fun warmSet() {
        for (size in SIZES) {
            val v = payload(size)
            val k = KevyEngine(root, "set"); val m = MmkvEngine("set")
            val keys = List(OPS) { "k${it % KEYS}" }
            cell(out, "set_warm", size, OPS,
                { timeNs { for (key in keys) k.set(key, v) } },
                { timeNs { for (key in keys) m.set(key, v) } })
        }
    }

    fun coldSet() {
        for (size in SIZES) {
            val v = payload(size)
            val k = KevyEngine(root, "cset"); val m = MmkvEngine("cset")
            val fresh = { r: Int -> List(OPS) { "r${r}k$it" } }
            cell(out, "set_cold", size, OPS,
                { r -> val keys = fresh(r); timeNs { for (key in keys) k.set(key, v) } },
                { r -> val keys = fresh(r); timeNs { for (key in keys) m.set(key, v) } })
        }
    }

    fun reopenedGet() {
        val size = 256; val v = payload(size)
        val k = KevyEngine(root, "rget"); val m = MmkvEngine("rget")
        val keys = List(OPS) { "k$it" }
        for (key in keys) { k.set(key, v); m.set(key, v) }
        cell(out, "get_reopened", size, OPS,
            { k.close(); k.open(); timeNs { for (key in keys) check(k.get(key)?.size == size) } },
            { m.close(); m.open(); timeNs { for (key in keys) check(m.get(key)?.size == size) } })
    }

    fun batch() {
        val size = 256; val v = payload(size); val n = 1000
        val k = KevyEngine(root, "batch"); val m = MmkvEngine("batch")
        val fresh = { r: Int -> List(n) { "r${r}k$it" } }
        cell(out, "batch_set", size, n,
            { r -> val keys = fresh(r); timeNs { k.mset(keys, v) } },
            { r -> val keys = fresh(r); timeNs { for (key in keys) m.set(key, v) } })
        val keys = fresh(0)
        cell(out, "batch_get", size, n,
            { timeNs { val got = k.mget(keys); check(got.size == n && got.all { it?.size == size }) } },
            { timeNs { for (key in keys) check(m.get(key)?.size == size) } })
    }

    fun startup() {
        val size = 256; val v = payload(size); val n = 10_000
        val k = KevyEngine(root, "boot"); val m = MmkvEngine("boot")
        for (i in 0 until n) { k.set("k$i", v); m.set("k$i", v) }
        val probe = "k${n - 1}"
        cell(out, "open_10k", size, 1,
            { k.close(); timeNs { k.open(); check(k.get(probe)?.size == size) } },
            { m.close(); timeNs { m.open(); check(m.get(probe)?.size == size) } })
        cell(out, "close_10k", size, 1,
            { val t = timeNs { k.close() }; k.open(); t },
            { val t = timeNs { m.close() }; m.open(); t })
    }
}
