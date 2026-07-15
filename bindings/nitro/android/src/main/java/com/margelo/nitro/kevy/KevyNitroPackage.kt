package com.margelo.nitro.kevy

import com.facebook.react.ReactPackage
import com.facebook.react.bridge.NativeModule
import com.facebook.react.bridge.ReactApplicationContext
import com.facebook.react.uimanager.ViewManager

// A pure-C++ Nitro HybridObject registers itself through the C++
// HybridObjectRegistry (via JNI_OnLoad in cpp-adapter.cpp), so it needs no
// TurboModule. But two things still require a Kotlin entry point:
//
//   1. Autolinking. expo-modules-autolinking only links an Android package
//      once it finds a `ReactPackage` class — a pure-C++ module has none,
//      so without this shim the gradle project is never included and the
//      C++ never builds. (RN's own community CLI is more lenient; Expo is
//      not, and this example runs under Expo autolinking.)
//   2. Loading libKevyNitro.so. Nitrogen generates KevyNitroOnLoad with the
//      System.loadLibrary call but nothing invokes it for a C++-only
//      module. Loading it here (when the package is constructed, before any
//      JS runs) triggers JNI_OnLoad → registerAllNatives(), so
//      NitroModules.createHybridObject("KevyNitro") resolves.
//
// The package itself contributes no native modules or views.
class KevyNitroPackage : ReactPackage {
  init {
    KevyNitroOnLoad.initializeNative()
  }

  override fun createNativeModules(
    reactContext: ReactApplicationContext
  ): List<NativeModule> = emptyList()

  override fun createViewManagers(
    reactContext: ReactApplicationContext
  ): List<ViewManager<*, *>> = emptyList()
}
