// The two engines behind one small surface. kevy opens a durable store
// (AOF, fsync everysec — the default); MMKV is its default mmap store.
package jp.golia.kevy.mmkvgate

import com.tencent.mmkv.MMKV
import jp.golia.kevy.KevyDB
import java.io.File
import java.util.UUID

class KevyEngine(root: File, tag: String) {
    private val dir = tmpDir(root, "kevy-$tag").path
    private var db = KevyDB.open(dir)

    fun set(k: String, v: ByteArray) = db.set(k, v)
    fun get(k: String): ByteArray? = db.get(k)

    fun mset(keys: List<String>, v: ByteArray) = db.mset(*Array(keys.size) { keys[it] to v })

    fun mget(keys: List<String>): List<ByteArray?> = db.mget(*keys.toTypedArray())

    fun close() = db.close()
    fun open() { db = KevyDB.open(dir) }
}

class MmkvEngine(tag: String) {
    private val id = "$tag-${UUID.randomUUID()}"
    private var m: MMKV = MMKV.mmkvWithID(id)

    fun set(k: String, v: ByteArray) = check(m.encode(k, v))
    fun get(k: String): ByteArray? = m.decodeBytes(k)

    fun close() = m.close()
    fun open() { m = MMKV.mmkvWithID(id) }
}
