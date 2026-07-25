# The Expo door's pod: the module shell plus the same Kevy.xcframework
# KevyKit wraps (scripts/prepare-native.sh copies it in — the authoritative
# build is packaging/apple/build-xcframework.sh).
require 'json'

package = JSON.parse(File.read(File.join(__dir__, '..', 'package.json')))

Pod::Spec.new do |s|
  s.name           = 'ExpoKevy'
  s.version        = package['version']
  s.summary        = package['description']
  s.description    = package['description']
  s.license        = package['license']
  s.author         = 'GOLIA K.K.'
  s.homepage       = package['homepage']
  s.platforms      = { :ios => '15.1' }
  s.swift_version  = '5.9'
  s.source         = { :git => 'https://github.com/goliajp/kevy.git' }
  s.static_framework = true

  s.dependency 'ExpoModulesCore'

  s.source_files = '*.swift'
  s.vendored_frameworks = 'Kevy.xcframework'
end
