// mmkvgate (Android) — kevy's embedded engine vs MMKV, as an app that
// runs every axis on launch and shows the table. It is built release
// (non-debuggable): a debuggable app runs its Kotlin far slower, and that
// cost would land on both engines' per-call overhead. An app rather than
// instrumented tests because a phone is driven through smix, which
// installs, launches and reads the screen. bench/mmkvgate/run-android.sh
// stages libkevy_jni.so and drives it.
plugins {
    id("com.android.application")
}

android {
    namespace = "jp.golia.kevy.mmkvgate"
    compileSdk = 36
    defaultConfig {
        applicationId = "jp.golia.kevy.mmkvgate"
        minSdk = 24
        targetSdk = 36
        ndk { abiFilters += "arm64-v8a" }
    }
    buildTypes {
        release {
            signingConfig = signingConfigs.getByName("debug")
        }
    }
    sourceSets {
        getByName("main") {
            java.srcDir("../../../../bindings/android/java")
            kotlin.srcDir("../../../../bindings/android/kevy/src/main/kotlin")
            jniLibs.srcDir("build/jni")
        }
    }
}

dependencies {
    implementation("com.tencent:mmkv:2.4.2")
}
