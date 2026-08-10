#
# flutter_kevy — a thin dart:ffi layer over the prebuilt kevy-ffi engine.
# No plugin-local C; the engine ships as kevy_ffi.xcframework, a DYNAMIC
# framework. CocoaPods embeds + code-signs a vendored dynamic framework
# into the app's Frameworks/, so dart:ffi DynamicLibrary.open(
# 'kevy_ffi.framework/kevy_ffi') resolves it at runtime via @rpath — the
# same shape as the Android door's dynamic .so. This sidesteps the
# static-xcframework path, where CocoaPods neither registered the Swift
# `import` module nor linked the `-lkevy_ffi` slice. KevyKit and expo
# keep the static xcframework (their Swift shells call the symbols
# directly at link time); flutter is the dynamic variant.
# scripts/prepare-native.sh builds + copies the dynamic xcframework in.
#
Pod::Spec.new do |s|
  s.name             = 'flutter_kevy'
  s.version          = '5.0.0'
  s.summary          = 'kevy embedded in Flutter over dart:ffi — TTL, structures, pub/sub, persistence you can read.'
  s.description      = 'kevy embedded in Flutter over dart:ffi, wrapping the kevy-ffi C ABI. The MMKV shape plus everything MMKV lacks.'
  s.homepage         = 'https://kevy.golia.jp'
  s.license          = { :file => '../LICENSE' }
  s.author           = 'GOLIA K.K.'
  s.source           = { :path => '.' }
  s.dependency 'Flutter'
  s.platform = :ios, '15.0'
  s.vendored_frameworks = 'kevy_ffi.xcframework'
  # A trivial source so CocoaPods builds a real framework target for this
  # pod; a vendored-only pod under use_frameworks! gets none, so the slice
  # is never extracted and the link fails "Framework 'kevy_ffi' not found".
  s.source_files = 'flutter_kevy.swift'
  s.swift_version = '5.0'
  s.pod_target_xcconfig = {
    'DEFINES_MODULE' => 'YES',
    'EXCLUDED_ARCHS[sdk=iphonesimulator*]' => 'i386',
  }
end
