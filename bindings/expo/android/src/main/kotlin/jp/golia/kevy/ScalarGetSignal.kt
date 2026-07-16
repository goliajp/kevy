// ScalarGetSignal — an internal control-flow signal, never seen by callers.
//
// The embedded scalar GET lane (KevyNative.get → kevy_get_shared) collapses a
// store error — GET on a non-string key, whose only error is WRONGTYPE — into
// an opaque `-2` that `null` can't tell apart from a miss. The JNI throws this
// (by name, jp/golia/kevy/ScalarGetSignal) to route that one case through the
// framed GET, where the proper -ERR WRONGTYPE surfaces.
//
// This class MUST exist on the door's classpath so the JNI FindClass resolves
// it instead of raising NoClassDefFoundError. It mirrors the java door's
// package-private ScalarGetSignal; KevyExpoModule catches it and translates it
// to the cross-platform KEVY_WRONGTYPE signal for src/index.ts.
package jp.golia.kevy

class ScalarGetSignal(message: String) : RuntimeException(message)
