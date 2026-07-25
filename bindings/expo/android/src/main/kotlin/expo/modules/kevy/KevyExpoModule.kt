// The Expo door, Android side — a bare shell over KevyNative (the raw JNI
// surface of libkevy_jni), mirroring the iOS shell function-for-function.
//
// Every function is synchronous JSI on the JS thread — the MMKV shape —
// so the handle maps below never see concurrent access. Handles are small
// ints (the JNI longs stay on this side of the bridge: a JS number is a
// double and would shear a 64-bit handle). RESP stays opaque bytes here;
// parsing and the typed surface live in src/index.ts.
package expo.modules.kevy

import expo.modules.kotlin.exception.CodedException
import expo.modules.kotlin.modules.Module
import expo.modules.kotlin.modules.ModuleDefinition
import jp.golia.kevy.KevyNative
import jp.golia.kevy.ScalarGetSignal

class KevyExpoModule : Module() {
    private val dbs = HashMap<Int, Long>()
    private val subs = HashMap<Int, Long>()
    private var nextId = 1

    private fun claim(): Int = nextId++

    private fun db(id: Int): Long =
        dbs[id] ?: throw IllegalStateException("kevy: closed handle")

    override fun definition() = ModuleDefinition {
        Name("Kevy")

        Function("open") { dir: String? ->
            val h = if (dir != null) KevyNative.open(dir.toByteArray()) else KevyNative.openMem()
            if (h == 0L) throw IllegalStateException("kevy: open failed")
            val id = claim()
            dbs[id] = h
            id
        }

        Function("close") { id: Int ->
            dbs.remove(id)?.let { KevyNative.close(it) }
        }

        // Deterministic teardown: fsync all shards, then refuse later
        // writes (reads stay). Idempotent.
        Function("shutdown") { id: Int ->
            if (KevyNative.shutdown(db(id)) != 0) {
                throw IllegalStateException("kevy: shutdown failed")
            }
        }

        // open with explicit durability/rewrite policy; opts = [fsync,
        // shards, rewritePct, rewriteMinSize, rewriteBytes,
        // rewriteIntervalSecs] (the TS layer packs it).
        Function("openWith") { dir: String?, opts: List<Double> ->
            val packed = LongArray(6) { opts[it].toLong() }
            val h = KevyNative.openWith(dir?.toByteArray(), packed)
            if (h == 0L) throw IllegalStateException("kevy: open failed")
            val id = claim()
            dbs[id] = h
            id
        }

        Function("cmd") { id: Int, packed: ByteArray ->
            KevyNative.cmd(db(id), packed)
                ?: throw IllegalStateException("kevy: kevy_cmd misuse")
        }

        // The boot-replay verdict, as [replayedCommands, replayedBytes,
        // elapsedMs, droppedBytes, corrupt(0/1), quarantineCount] — the TS
        // layer maps it to a typed object. Doubles: JS numbers either way.
        Function("openReport") { id: Int ->
            val r = KevyNative.openReport(db(id))
                ?: throw IllegalStateException("kevy: open_report misuse")
            r.map { it.toDouble() }
        }

        Function("get") { id: Int, key: ByteArray ->
            try {
                KevyNative.get(db(id), key)
            } catch (signal: ScalarGetSignal) {
                // The scalar lane can't carry a typed store error (GET on a
                // non-string key). Translate the JNI signal to the cross-platform
                // KEVY_WRONGTYPE marker so src/index.ts re-runs the framed GET,
                // which surfaces -ERR WRONGTYPE. Mirrors iOS's wrongType().
                throw CodedException(
                    "KEVY_WRONGTYPE: scalar GET on a non-string key; use the framed GET",
                    signal,
                )
            }
        }

        Function("set") { id: Int, key: ByteArray, value: ByteArray, ttlMs: Double ->
            val rc = KevyNative.set(db(id), key, value, if (ttlMs > 0) ttlMs.toLong() else 0L)
            if (rc != 0) throw IllegalStateException("kevy: kevy_set misuse")
        }

        Function("subscribe") { id: Int, chan: ByteArray, pattern: Boolean ->
            val h = KevyNative.subscribe(db(id), chan, pattern)
            if (h == 0L) throw IllegalStateException("kevy: subscribe failed")
            val subId = claim()
            subs[subId] = h
            subId
        }

        Function("subNext") { subId: Int ->
            val h = subs[subId] ?: throw IllegalStateException("kevy: closed handle")
            KevyNative.subNext(h)
        }

        Function("subClose") { subId: Int ->
            subs.remove(subId)?.let { KevyNative.subClose(it) }
        }

        Function("version") { ->
            String(KevyNative.version(), Charsets.UTF_8)
        }
    }
}
