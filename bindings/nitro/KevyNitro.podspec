require "json"

package = JSON.parse(File.read(File.join(__dir__, "package.json")))

Pod::Spec.new do |s|
  s.name         = "KevyNitro"
  s.version      = package["version"]
  s.summary      = package["description"]
  s.homepage     = "https://kevy.golia.jp"
  s.license      = package["license"]
  s.authors      = "GOLIA K.K."
  # ExpoModulesCore (present in the Expo example's Pods) requires >= 16.4.
  s.platforms    = { :ios => "16.4" }
  s.source       = { :git => "https://github.com/goliajp/kevy.git" }
  s.static_framework = true

  # The C++ HybridObject implementation plus the local kevy.h it includes.
  s.source_files = "cpp/**/*.{h,hpp,cpp}"

  # The prebuilt kevy engine C ABI — the same Kevy.xcframework KevyKit and
  # ExpoKevy vendor (built by packaging/apple/build-xcframework.sh). It
  # carries the kevy_* symbols the C++ door links against, mirroring the way
  # the Android side links libkevy_ffi.so.
  s.vendored_frameworks = "ios/KevyEngine.xcframework"

  s.pod_target_xcconfig = {
    # So the generated Objective-C++ autolinking .mm finds HybridKevyNitro.hpp
    # / kevy.h next to the C++ sources.
    "HEADER_SEARCH_PATHS" => "\"$(PODS_TARGET_SRCROOT)/cpp\"",
  }

  # Nitrogen-generated specs, Swift<->C++ bridges, and the +load registration
  # that wires "KevyNitro" into the HybridObjectRegistry. Also adds the
  # NitroModules dependency.
  load File.join(__dir__, "nitrogen", "generated", "ios", "KevyNitro+autolinking.rb")
  add_nitrogen_files(s)

  s.dependency "React-jsi"
  s.dependency "React-callinvoker"
  install_modules_dependencies(s)
end
