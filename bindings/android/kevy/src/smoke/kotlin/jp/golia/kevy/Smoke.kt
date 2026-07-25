// The Kotlin smoke for the typed KevyDB shell — the scalar lane's WRONGTYPE
// fidelity in particular. Smoke.java drives the raw KevyNative surface via
// cmd(); it never touches KevyDB.get(), so it can't see the scalar lane fold a
// wrong-type key into a leaked ScalarGetSignal / NoClassDefFoundError instead
// of a typed error — that's why the bug was latent. This drives KevyDB.get()
// directly: a GET on a list key must surface a typed WRONGTYPE KevyException,
// routed through the framed GET, while the happy scalar path (set/get round
// trip, miss = null) still answers straight off the fast lane.
//
// Build + run (from the repo root, needs kotlinc; wired into bench/jnigate.sh):
//   cargo build -p kevy-jni
//   javac -d /tmp/kevy-kt/classes bindings/android/java/jp/golia/kevy/*.java
//   kotlinc -cp /tmp/kevy-kt/classes -include-runtime -d /tmp/kevy-kt/kevy.jar \
//       bindings/android/kevy/src/main/kotlin/jp/golia/kevy/*.kt \
//       bindings/android/kevy/src/smoke/kotlin/jp/golia/kevy/*.kt
//   java -Djava.library.path=target/debug \
//       -cp /tmp/kevy-kt/classes:/tmp/kevy-kt/kevy.jar jp.golia.kevy.SmokeKt
package jp.golia.kevy

import kotlin.system.exitProcess

private fun fail(msg: String): Nothing {
    System.err.println("FAIL $msg")
    exitProcess(1)
}

fun main() {
    KevyDB.openInMemory().use { db ->
        // Happy scalar path: the fast lane still round-trips and reports a
        // miss as null (not a throw).
        db.set("s:k", "v1")
        val hit = db.getText("s:k") ?: fail("scalar get on a live key returned null")
        if (hit != "v1") fail("scalar get round trip: got '$hit'")
        if (db.get("s:none") != null) fail("scalar get miss should be null")

        // Scalar WRONGTYPE: GET on a list key must NOT swallow the wrong type
        // into null, nor leak the native ScalarGetSignal / a NoClassDefFoundError
        // — it has to surface the same typed WRONGTYPE KevyException that
        // cmd("GET", …) raises, via the framed fallback.
        db.cmd("RPUSH", "wrong:k", "a")
        try {
            db.get("wrong:k")
            fail("scalar get on a list key did not throw a typed WRONGTYPE")
        } catch (e: KevyException) {
            val m = e.message ?: ""
            if (!m.contains("WRONGTYPE")) fail("scalar get wrong-type not typed: '$m'")
        }
        // getText() delegates to get(), so it inherits the same fidelity.
        try {
            db.getText("wrong:k")
            fail("getText on a list key did not throw a typed WRONGTYPE")
        } catch (e: KevyException) {
            if (!(e.message ?: "").contains("WRONGTYPE")) fail("getText wrong-type not typed")
        }
    }
    println("smoke-kt: ok")
}
