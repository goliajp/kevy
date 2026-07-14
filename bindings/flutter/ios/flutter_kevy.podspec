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

  # KevyAnchor.swift `import Kevy`s the xcframework's clang module — the
  # same mechanism the (green) expo door uses — so CocoaPods links the
  # vendored static xcframework and its symbols survive into the app
  # binary, where DynamicLibrary.process() finds them at runtime. A bare-C
  # extern does not trigger the pod's module link; the module import does.
  s.source_files = 'KevyAnchor.swift'
  s.pod_target_xcconfig = {
    'DEFINES_MODULE' => 'YES',
    'EXCLUDED_ARCHS[sdk=iphonesimulator*]' => 'i386',
  }
  s.swift_version = '5.0'
end
