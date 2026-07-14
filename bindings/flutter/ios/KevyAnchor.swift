// Link anchor for the dart:ffi door, matching the expo door's mechanism.
//
// flutter_kevy reaches the engine through Dart's DynamicLibrary.process(),
// which resolves symbols at runtime — so at link time nothing references
// the vendored Kevy.xcframework and CocoaPods neither links it nor keeps
// its symbols ("Library 'kevy_ffi' not found" at link). Importing the
// `Kevy` clang module (the same `import Kevy` the green expo door's Swift
// shell uses) is what makes CocoaPods link the xcframework; the forced
// reference to kevy_abi keeps its symbols in the app binary for
// DynamicLibrary.process() to find. A bare-C extern does not trigger the
// pod's module link — the module import does.
import Foundation
import Kevy

@objc public final class KevyLinkAnchor: NSObject {
    // Never called at runtime — its only job is to be a link-time
    // reference so the engine's symbols survive into the binary.
    @objc public static func anchor() -> UInt32 {
        return kevy_abi()
    }
}
