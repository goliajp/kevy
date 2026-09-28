// Timing and reporting shared by every axis — the same shape as the
// Apple harness: both engines run in alternating rounds inside one axis,
// each figure is the median of its rounds after one discarded warm-up,
// and every cell is one MMKVGATE line, logged and kept for the screen.
package jp.golia.kevy.mmkvgate

import android.util.Log
import java.io.File
import java.util.UUID

const val ROUNDS = 7

fun payload(n: Int) = ByteArray(n) { 0x61 }

fun tmpDir(root: File, tag: String): File =
    File(root, "mmkvgate-$tag-${UUID.randomUUID()}").apply { mkdirs() }

inline fun timeNs(body: () -> Unit): Long {
    val t0 = System.nanoTime()
    body()
    return System.nanoTime() - t0
}

fun cell(
    out: MutableList<String>,
    axis: String,
    bytes: Int,
    ops: Int,
    kevy: (Int) -> Long,
    mmkv: (Int) -> Long,
) {
    kevy(-1)
    mmkv(-1)
    val k = LongArray(ROUNDS)
    val m = LongArray(ROUNDS)
    for (r in 0 until ROUNDS) {
        k[r] = kevy(r)
        m[r] = mmkv(r)
    }
    k.sort()
    m.sort()
    val kn = k[ROUNDS / 2].toDouble() / ops
    val mn = m[ROUNDS / 2].toDouble() / ops
    val line = "MMKVGATE %s %d kevy_ns=%.1f mmkv_ns=%.1f kevy/mmkv=%.3f"
        .format(java.util.Locale.ROOT, axis, bytes, kn, mn, kn / mn)
    Log.i("MMKVGATE", line)
    out.add(line)
}
