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

  # kevy_anchor.c forces a link-time reference to one engine symbol so the
  # linker keeps the vendored static xcframework in the binary — without
  # it, DynamicLibrary.process() (which resolves at runtime) leaves the
  # library unreferenced and it gets dead-stripped.
  s.source_files = 'kevy_anchor.c'
  s.pod_target_xcconfig = {
    'DEFINES_MODULE' => 'YES',
    'EXCLUDED_ARCHS[sdk=iphonesimulator*]' => 'i386',
  }
  s.swift_version = '5.0'
end
