// Expo-modules-wired AppDelegate for a BARE React Native app.
//
// Mirrors what `expo prebuild -p ios` generates for RN 0.86 / SDK 57: the
// app delegate subclasses `ExpoAppDelegate` and boots React Native through
// `ExpoReactNativeFactory` (not the plain `RCTReactNativeFactory`). That is
// what attaches Expo's module registry to the bridge so autolinked Expo
// modules — ExpoModulesCore and expo-kevy — are reachable from JS.
//
// The bare specifics are preserved: the registered module name is "BareKevy"
// (index.js → AppRegistry.registerComponent) and the debug bundle root is
// "index" (this app's entry point, not the managed `.expo/.virtual-metro-entry`).
internal import Expo
import React
import ReactAppDependencyProvider

@main
class AppDelegate: ExpoAppDelegate {
  var window: UIWindow?

  var reactNativeDelegate: ExpoReactNativeFactoryDelegate?
  var reactNativeFactory: RCTReactNativeFactory?

  public override func application(
    _ application: UIApplication,
    didFinishLaunchingWithOptions launchOptions: [UIApplication.LaunchOptionsKey: Any]? = nil
  ) -> Bool {
    let delegate = ReactNativeDelegate()
    let factory = ExpoReactNativeFactory(delegate: delegate)
    delegate.dependencyProvider = RCTAppDependencyProvider()

    reactNativeDelegate = delegate
    reactNativeFactory = factory

#if os(iOS) || os(tvOS)
    window = UIWindow(frame: UIScreen.main.bounds)
    factory.startReactNative(
      withModuleName: "BareKevy",
      in: window,
      launchOptions: launchOptions)
#endif

    return super.application(application, didFinishLaunchingWithOptions: launchOptions)
  }
}

class ReactNativeDelegate: ExpoReactNativeFactoryDelegate {
  override func sourceURL(for bridge: RCTBridge) -> URL? {
    // needed to return the correct URL for expo-dev-client.
    bridge.bundleURL ?? bundleURL()
  }

  override func bundleURL() -> URL? {
#if DEBUG
    return RCTBundleURLProvider.sharedSettings().jsBundleURL(forBundleRoot: "index")
#else
    return Bundle.main.url(forResource: "main", withExtension: "jsbundle")
#endif
  }
}
