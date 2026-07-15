# barern-example — expo-kevy in a BARE React Native app

Proof that `expo-kevy` works in a **plain** React Native app — one created
with `@react-native-community/cli` (NOT `expo`), with `expo-modules-core`
added so Expo modules autolink. This is the standard "Expo modules in bare
RN" path, and it runs the **same** on-device smoke the Expo example runs
(SET/GET, TTL, INCRBY, REOPEN-durability, version → `MOBILEGATE:PASS`).

The app was scaffolded with:

```
npx @react-native-community/cli init BareKevy --version 0.86.0 --directory barern-example
```

New architecture is on (`newArchEnabled=true`), Hermes on — matching the repo.

## Prerequisites

The engine's Android native lib and the expo-kevy Android artifacts must be
vendored first (same as the Expo example — they are `.gitignore`d):

```sh
# from the repo root
packaging/android/build-jnilibs.sh        # cargo cross-compile → libkevy_jni.so (arm64-v8a + x86_64)
# then copy the Android artifacts into the expo-kevy module (Android-only slice
# of bindings/expo/scripts/prepare-native.sh):
#   cp bindings/android/java/jp/golia/kevy/KevyNative.java bindings/expo/android/src/main/java/jp/golia/kevy/
#   cp target/<triple>/release/libkevy_jni.so bindings/expo/android/src/main/jniLibs/<abi>/
```

Then in this folder:

```sh
npm install
npm run android          # or: bash ../../../bench/mobilegate.sh barern android
```

## How expo-modules-core autolinking is wired (manual, RN 0.86)

`npx install-expo-modules@latest` does **not** work on RN 0.86 — its Expo-SDK
version map predates SDK 57 (`Unable to find compatible Expo SDK version`,
and `--sdk-version 57.0.0` → `Unsupported sdkVersion`). So the wiring here is
the manual path, mirroring exactly what `expo prebuild` generates for SDK 57 /
RN 0.86:

- **`android/settings.gradle`** — `pluginManagement` includes the
  `expo-gradle-plugin` composite build; `plugins { id("expo-autolinking-settings") }`;
  the React autolink command is swapped to `expoAutolinking.rnConfigCommand`
  (so Expo modules are discovered); `expoAutolinking.useExpoModules()` +
  `useExpoVersionCatalog()`.
- **`android/build.gradle`** — `apply plugin: "expo-root-project"` + jitpack repo.
- **`MainApplication.kt`** — `ExpoReactHostFactory.getDefaultReactHost` +
  `ApplicationLifecycleDispatcher` hooks.
- **`MainActivity.kt`** — delegate wrapped in `ReactActivityDelegateWrapper`.

Two additional bare-RN fixes were needed that a managed Expo app gets for free:

1. **`metro.config.js`** — `expo-kevy` is a `file:..` dependency, which npm
   links as a symlink pointing at `bindings/expo`, OUTSIDE this project root.
   Metro only watches the project root by default, so the symlink target is
   added to `watchFolders`, and the sibling Expo `example/` app (its own
   react-native copy) is added to `resolver.blockList` to avoid duplicate
   Haste modules.
2. **`index.js`** — `import 'expo';` installs Expo's "winter" runtime
   polyfills. expo-kevy decodes RESP bytes with `TextDecoder`, which Hermes
   does not provide on its own; the managed Expo entry point loads this
   implicitly, a bare app must import it.

## Scope

Only **Android** was wired and validated (per the task — on `emulator-5554`).
The `ios/` folder is the untouched community-CLI scaffold: its `Podfile` is
NOT wired for Expo modules, so iOS is not expected to build as-is.
