// ScalarGetSignal — an internal control-flow signal, never seen by callers.
// The embedded scalar GET lane (KevyNative.get / kevy_get_shared) collapses a
// store error — GET on a non-string key, whose only error is WRONGTYPE — into
// an opaque `-2` that `null` can't tell apart from a miss. The native throws
// this to route that one case through the framed GET, where the proper typed
// WRONGTYPE StoreException surfaces (matching the remote backend). It is caught
// in EmbeddedBackend.fastGet and turned into that framed reply, so it must not
// escape the package.
package jp.golia.kevy;

final class ScalarGetSignal extends RuntimeException {
    ScalarGetSignal(String message) {
        super(message);
    }
}
