#
# flutter_kevy — a thin dart:ffi layer over the prebuilt kevy-ffi engine.
# No plugin-local C; the engine ships as Kevy.xcframework (the same one
# KevyKit and expo-kevy vendor), linked so its symbols land in the app
# binary — DynamicLibrary.process() finds them. scripts/prepare-native.sh
# copies the xcframework in.
#
Pod::Spec.new do |s|
  s.name             = 'flutter_kevy'
  s.version          = '4.0.0'
  s.summary          = 'kevy embedded in Flutter over dart:ffi — TTL, structures, pub/sub, persistence you can read.'
  s.description      = 'kevy embedded in Flutter over dart:ffi, wrapping the kevy-ffi C ABI. The MMKV shape plus everything MMKV lacks.'
  s.homepage         = 'https://kevy.golia.jp'
  s.license          = { :file => '../LICENSE' }
  s.author           = 'GOLIA K.K.'
  s.source           = { :path => '.' }
  s.dependency 'Flutter'
  s.platform = :ios, '15.0'
  s.vendored_frameworks = 'Kevy.xcframework'

  # A static-library XCFramework must be force-loaded so its symbols reach
  # the process image for DynamicLibrary.process() — a dead-strip would
  # drop them otherwise (dart:ffi references them only at runtime). The
  # slice differs by SDK: device vs simulator.
  s.pod_target_xcconfig = {
    'DEFINES_MODULE' => 'YES',
    'EXCLUDED_ARCHS[sdk=iphonesimulator*]' => 'i386',
    'OTHER_LDFLAGS[sdk=iphoneos*]' => '-force_load ${PODS_TARGET_SRCROOT}/Kevy.xcframework/ios-arm64/libkevy_ffi.a',
    'OTHER_LDFLAGS[sdk=iphonesimulator*]' => '-force_load ${PODS_TARGET_SRCROOT}/Kevy.xcframework/ios-arm64-simulator/libkevy_ffi.a',
  }
  s.swift_version = '5.0'
end
