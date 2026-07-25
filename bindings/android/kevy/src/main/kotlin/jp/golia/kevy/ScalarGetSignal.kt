// ScalarGetSignal — an internal control-flow signal, never seen by callers.
// The embedded scalar GET lane (KevyNative.get / kevy_get_shared) collapses a
// store error — GET on a non-string key, whose only error is WRONGTYPE — into
// an opaque signal that `null` can't tell apart from a miss. The native throws
// this (by its FQN jp.golia.kevy.ScalarGetSignal) to route that one case
// through the framed GET, where the proper typed WRONGTYPE KevyException
// surfaces — matching cmd("GET", …) and every remote backend. KevyDB.get()
// catches it and re-runs the framed GET, so it must never escape the package.
//
// Its only reason to exist is to be found: KevyNative.get's JNI FindClass looks
// this class up by that exact name. Without it, FindClass fails and leaves a
// pending NoClassDefFoundError that bubbles to the caller instead of a typed
// error. `internal` narrows Kotlin visibility; the compiled class stays public
// (unmangled) so the JNI lookup still resolves.
package jp.golia.kevy

internal class ScalarGetSignal(message: String) : RuntimeException(message)
