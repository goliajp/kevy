// A trivial compile unit so CocoaPods builds flutter_kevy as a real
// framework target. Without a source file, a vendored-only pod under
// `use_frameworks!` gets no build target, so the "[CP] Copy XCFrameworks"
// phase never extracts the kevy_ffi slice and the link fails with
// "Framework 'kevy_ffi' not found". This file references nothing from the
// engine: the dynamic kevy_ffi.framework is embedded + signed into the
// app whole (no dead-strip, unlike a static archive), and dart:ffi
// DynamicLibrary.open('kevy_ffi.framework/kevy_ffi') resolves it at
// runtime via @rpath.
import Foundation

@objc public final class FlutterKevyPlugin: NSObject {}
