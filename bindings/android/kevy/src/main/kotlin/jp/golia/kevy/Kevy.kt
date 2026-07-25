// Kevy — kevy embedded, Kotlin-shaped.
//
// The typed methods mirror the other kevy packages (set/get/del/incrBy/…)
// and throw KevyException on a protocol error — a typed call has one
// meaning, so "-WRONGTYPE" is a failure. cmd() reaches all 184 verbs and
// returns protocol errors as values (KevyValue.Error).
//
//   val db = KevyDB.open(context.filesDir.resolve("kevy").path)
//   db.set("session:7f3a", token, ttlMs = 3_600_000)
//   db.getText("session:7f3a")
//
// Pub/sub is a polled pump: subscribe(...) registers a callback, poll()
// drains every queue and dispatches on the calling thread — drive it from
// your looper/timer, wasm-package style.
package jp.golia.kevy

/** Thrown for ABI misuse and for protocol errors on the typed surface. */
class KevyException(message: String) : RuntimeException(message)

/** One node of a parsed RESP reply. */
sealed class KevyValue {
    data class Simple(val text: String) : KevyValue()
    data class Error(val message: String) : KevyValue()
    data class Int64(val value: Long) : KevyValue()
    class Bulk(val bytes: ByteArray) : KevyValue() {
        override fun equals(other: Any?) = other is Bulk && bytes.contentEquals(other.bytes)
        override fun hashCode() = bytes.contentHashCode()
    }
    object Nil : KevyValue()
    data class Array(val items: List<KevyValue>) : KevyValue()

    /** Integer payload, else null. */
    val asLong: Long?
        get() = (this as? Int64)?.value

    /** Bulk / simple payload as UTF-8 text, else null. */
    val asText: String?
        get() = when (this) {
            is Simple -> text
            is Bulk -> bytes.toString(Charsets.UTF_8)
            else -> null
        }
}

/** RESP decode: one complete reply in, a KevyValue tree out. */
internal object Resp {
    fun parse(reply: ByteArray): KevyValue = Cursor(reply).one()

    private class Cursor(private val b: ByteArray) {
        private var pos = 0

        fun one(): KevyValue {
            if (pos >= b.size) throw KevyException("kevy: truncated RESP reply")
            val tag = b[pos].toInt().toChar()
            pos += 1
            val head = line()
            return when (tag) {
                '+' -> KevyValue.Simple(head)
                '-' -> KevyValue.Error(head)
                ':' -> KevyValue.Int64(head.toLong())
                '$' -> bulk(head.toInt())
                '*' -> array(head.toInt())
                else -> throw KevyException("kevy: unknown RESP tag '$tag'")
            }
        }

        private fun line(): String {
            val start = pos
            while (pos + 1 < b.size && !(b[pos].toInt() == 13 && b[pos + 1].toInt() == 10)) {
                pos += 1
            }
            if (pos + 1 >= b.size) throw KevyException("kevy: truncated RESP reply")
            val s = String(b, start, pos - start, Charsets.UTF_8)
            pos += 2
            return s
        }

        private fun bulk(n: Int): KevyValue {
            if (n < 0) return KevyValue.Nil
            if (pos + n + 2 > b.size) throw KevyException("kevy: truncated RESP reply")
            val d = b.copyOfRange(pos, pos + n)
            pos += n + 2
            return KevyValue.Bulk(d)
        }

        private fun array(n: Int): KevyValue {
            if (n < 0) return KevyValue.Nil
            val items = ArrayList<KevyValue>(n)
            repeat(n) { items.add(one()) }
            return KevyValue.Array(items)
        }
    }
}

/** Explicit durability/rewrite policy for [KevyDB.open]. The defaults are
 *  exactly what the options-less open uses. [rewritePct] 0 turns the
 *  auto-rewrite growth rule off (as in Redis); [rewriteBytes] /
 *  [rewriteIntervalSecs] are the absolute-size and staleness triggers
 *  (0 = off). */
data class KevyOpenOptions(
    /** AOF fsync policy: 0 everysec, 1 always, 2 no. */
    val fsync: Int = 0,
    /** Keyspace shards (0 = engine default). */
    val shards: Int = 0,
    /** Auto-rewrite growth trigger, percent (0 = rule off). */
    val rewritePct: Int = 100,
    /** Growth rule's minimum size gate, bytes. */
    val rewriteMinSize: Long = 64L * 1024 * 1024,
    /** Absolute-size rewrite trigger, bytes (0 = off). */
    val rewriteBytes: Long = 0,
    /** Staleness rewrite trigger, seconds (0 = off). */
    val rewriteIntervalSecs: Long = 0,
)

/** The embedded engine. One instance owns its data directory. */
class KevyDB private constructor(private var handle: Long) : AutoCloseable {

    companion object {
        /** Open a persistent store rooted at [dir], replaying its log. */
        fun open(dir: String): KevyDB {
            val h = KevyNative.open(dir.toByteArray(Charsets.UTF_8))
            if (h == 0L) throw KevyException("kevy: open failed: $dir")
            return KevyDB(h)
        }

        /** [open] with explicit [options]; [dir] null = in-memory. */
        fun open(dir: String?, options: KevyOpenOptions): KevyDB {
            val opts = longArrayOf(
                options.fsync.toLong(),
                options.shards.toLong(),
                options.rewritePct.toLong(),
                options.rewriteMinSize,
                options.rewriteBytes,
                options.rewriteIntervalSecs,
            )
            val h = KevyNative.openWith(dir?.toByteArray(Charsets.UTF_8), opts)
            if (h == 0L) throw KevyException("kevy: open failed: ${dir ?: "(in-memory)"}")
            return KevyDB(h)
        }

        /** Open a pure in-memory store — nothing survives the process. */
        fun openInMemory(): KevyDB {
            val h = KevyNative.openMem()
            if (h == 0L) throw KevyException("kevy: open failed (in-memory)")
            return KevyDB(h)
        }
    }

    private val subs = mutableListOf<KevySubscription>()

    private fun live(): Long {
        if (handle == 0L) throw KevyException("kevy: used after close")
        return handle
    }

    // ── the escape hatch: every verb, protocol errors as values ────────

    /** Run one command; args[0] is the verb. */
    fun cmd(vararg args: String): KevyValue =
        cmdBytes(args.map { it.toByteArray(Charsets.UTF_8) })

    /** cmd() for binary-safe arguments. */
    fun cmdBytes(args: List<ByteArray>): KevyValue {
        if (args.isEmpty()) throw KevyException("kevy: empty argv")
        val reply = KevyNative.cmd(live(), KevyNative.pack(*args.toTypedArray()))
            ?: throw KevyException("kevy: cmd misuse")
        return Resp.parse(reply)
    }

    // ── the typed surface (mirrors the other kevy packages) ────────────

    /** Store [value] under [key]; ttlMs 0 = no expiry. Scalar fast path —
     *  no argv assembly, no RESP framing. */
    fun set(key: String, value: ByteArray, ttlMs: Long = 0) {
        val rc = KevyNative.set(live(), key.toByteArray(Charsets.UTF_8), value, ttlMs)
        if (rc != 0) throw KevyException("kevy: set failed ($rc)")
    }

    /** Store UTF-8 [value] under [key]. */
    fun set(key: String, value: String, ttlMs: Long = 0) {
        set(key, value.toByteArray(Charsets.UTF_8), ttlMs)
    }

    /** Fetch [key]; null on a miss. Scalar fast path.
     *
     *  The scalar lane can't carry a typed store error, so on a WRONGTYPE
     *  (GET on a non-string key) the native throws [ScalarGetSignal]. Catch
     *  it and re-run the framed GET so a typed [KevyException] (WRONGTYPE)
     *  surfaces — the same failure cmd("GET", …) and every other kevy backend
     *  raise, rather than folding the wrong type into a null miss. */
    fun get(key: String): ByteArray? =
        try {
            KevyNative.get(live(), key.toByteArray(Charsets.UTF_8))
        } catch (signal: ScalarGetSignal) {
            (want(cmd("GET", key)) as? KevyValue.Bulk)?.bytes
        }

    /** Fetch [key] as UTF-8 text; null on a miss. */
    fun getText(key: String): String? = get(key)?.toString(Charsets.UTF_8)

    /** Remove keys, returning how many existed. */
    fun del(vararg keys: String): Long = want(cmd("DEL", *keys)).asLong ?: 0

    /** Add [delta] to the integer at [key], returning the result. */
    fun incrBy(key: String, delta: Long = 1): Long =
        want(cmd("INCRBY", key, delta.toString())).asLong ?: 0

    /** Set [key]'s time-to-live; false when the key does not exist. */
    fun expire(key: String, ttlMs: Long): Boolean =
        want(cmd("PEXPIRE", key, ttlMs.toString())).asLong == 1L

    /** Remaining life in ms; Redis semantics (-1 no TTL, -2 no key). */
    fun pttlMs(key: String): Long = want(cmd("PTTL", key)).asLong ?: -2

    /** Keys matching a glob pattern. Page with cmd("SCAN", …) on a large
     *  keyspace. */
    fun keys(pattern: String = "*"): List<String> {
        val v = want(cmd("KEYS", pattern)) as? KevyValue.Array ?: return emptyList()
        return v.items.mapNotNull { it.asText }
    }

    /** Values for [keys], null per missing key. */
    fun mget(vararg keys: String): List<ByteArray?> {
        val v = want(cmd("MGET", *keys)) as? KevyValue.Array ?: return emptyList()
        return v.items.map { (it as? KevyValue.Bulk)?.bytes }
    }

    /** Live key count. */
    fun dbsize(): Long = want(cmd("DBSIZE")).asLong ?: 0

    /** Empty the store. */
    fun flushall() {
        want(cmd("FLUSHALL"))
    }

    /** The boot-replay verdict: what this open restored — and what it could
     *  not. [KevyOpenStats.droppedBytes] > 0 or [KevyOpenStats.corrupt] means
     *  the store recovered LESS than its files held (the dropped region was
     *  quarantined next to the AOF): surface it as a startup health check
     *  instead of scraping the boot WARN line from logcat. */
    fun openReport(): KevyOpenStats {
        val r = KevyNative.openReport(live())
            ?: throw KevyException("kevy: openReport misuse")
        return KevyOpenStats(
            replayedCommands = r[0],
            replayedBytes = r[1],
            elapsedMs = r[2],
            droppedBytes = r[3],
            corrupt = r[4] != 0L,
            quarantineCount = r[5],
        )
    }

    /** Deliver [payload] to every in-process subscriber on [channel];
     *  returns the receiver count. */
    fun publish(channel: String, payload: ByteArray): Long {
        val argv = listOf(
            "PUBLISH".toByteArray(Charsets.UTF_8),
            channel.toByteArray(Charsets.UTF_8),
            payload,
        )
        return want(cmdBytes(argv)).asLong ?: 0
    }

    /** publish() for UTF-8 text payloads. */
    fun publish(channel: String, payload: String): Long =
        publish(channel, payload.toByteArray(Charsets.UTF_8))

    // ── pub/sub: callback registration + poll pump ─────────────────────

    /** Register [callback] for [channel] (a glob when [pattern]). Frames
     *  are delivered by [poll], on whichever thread calls it. */
    fun subscribe(
        channel: String,
        pattern: Boolean = false,
        callback: (payload: ByteArray, channel: String) -> Unit,
    ): KevySubscription {
        val h = KevyNative.subscribe(live(), channel.toByteArray(Charsets.UTF_8), pattern)
        if (h == 0L) throw KevyException("kevy: subscribe failed: $channel")
        val sub = KevySubscription(h, callback)
        subs.add(sub)
        return sub
    }

    /** Drain every subscription queue, dispatching callbacks. Returns the
     *  number of frames drained. Call from your looper/timer. */
    fun poll(): Int {
        var n = 0
        for (s in subs) n += s.poll()
        subs.removeAll { it.isClosed }
        return n
    }

    /** Deterministic teardown: flush every shard's AOF (a REAL fsync),
     *  write the feed continuity marker, then refuse every later write.
     *  Reads stay available, so the handle stays live — [close] when done.
     *  Idempotent; a lifecycle hook's exit is `db.shutdown()` then
     *  terminate. Throws on an I/O failure (the store is still usable;
     *  retry or exit). */
    fun shutdown() {
        val rc = KevyNative.shutdown(live())
        if (rc != 0) throw KevyException("kevy: shutdown failed ($rc)")
    }

    /** Close the store. Safe to call more than once. */
    override fun close() {
        for (s in subs) s.close()
        subs.clear()
        if (handle != 0L) {
            KevyNative.close(handle)
            handle = 0
        }
    }

    private fun want(v: KevyValue): KevyValue {
        if (v is KevyValue.Error) throw KevyException("kevy: ${v.message}")
        return v
    }
}

/** A polled subscription: [poll] drains pending frames into the callback,
 *  [close] unsubscribes. Owned by the [KevyDB] that opened it. */
class KevySubscription internal constructor(
    private var handle: Long,
    private val callback: (payload: ByteArray, channel: String) -> Unit,
) : AutoCloseable {

    val isClosed: Boolean
        get() = handle == 0L

    /** Drain pending frames, invoking the callback for each message /
     *  pmessage (subscribe acks are consumed silently). Returns the number
     *  of frames drained. */
    fun poll(): Int {
        var n = 0
        while (handle != 0L) {
            val raw = KevyNative.subNext(handle) ?: break
            n += 1
            dispatch(Resp.parse(raw))
        }
        return n
    }

    private fun dispatch(frame: KevyValue) {
        val items = (frame as? KevyValue.Array)?.items ?: return
        when (items.firstOrNull()?.asText) {
            // *3: message <channel> <payload>
            "message" -> if (items.size == 3) {
                val chan = items[1].asText ?: return
                val payload = (items[2] as? KevyValue.Bulk)?.bytes ?: return
                callback(payload, chan)
            }
            // *4: pmessage <pattern> <channel> <payload>
            "pmessage" -> if (items.size == 4) {
                val chan = items[2].asText ?: return
                val payload = (items[3] as? KevyValue.Bulk)?.bytes ?: return
                callback(payload, chan)
            }
        }
    }

    override fun close() {
        if (handle != 0L) {
            KevyNative.subClose(handle)
            handle = 0
        }
    }
}

/** The boot-replay verdict [KevyDB.openReport] returns — the typed face of
 *  the JNI gate's long[6]. */
data class KevyOpenStats(
    /** Commands replayed from the AOF(s), summed across shards. */
    val replayedCommands: Long,
    /** Bytes actually replayed (the valid prefixes). */
    val replayedBytes: Long,
    /** Wall-clock time of the startup replay, in milliseconds. */
    val elapsedMs: Long,
    /** Bytes dropped past the last replayable frame (quarantined on disk). */
    val droppedBytes: Long,
    /** True when any shard's replay stopped at a corrupt frame. */
    val corrupt: Boolean,
    /** Quarantine files written by the open's repair. */
    val quarantineCount: Long,
)
